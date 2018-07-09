//! OrderInfos Services, presents CRUD operations with order_info

use diesel::connection::AnsiTransactionManager;
use diesel::pg::Pg;
use diesel::Connection;
use failure::Error as FailureError;
use failure::Fail;
use futures::Future;
use futures_cpupool::CpuPool;
use hyper::Post;
use r2d2::{ManageConnection, Pool};
use serde_json;

use stq_http::client::ClientHandle;

use super::types::ServiceFuture;
use errors::Error;
use models::{BillingOrder, CallbackId, CreateInvoicePayload, CreateInvoice, Invoice, NewInvoice, NewOrderInfo, SubjectIdentifier, UserId};
use repos::repo_factory::ReposFactory;
use repos::RepoResult;

type URL = String;

pub trait OrderInfoService {
    /// Creates invoice in billing system
    fn create_invoice(&self, create_order: CreateInvoice) -> ServiceFuture<URL>;
    /// Creates orders in billing system, returning url for payment
    fn set_paid(&self, callback_id: CallbackId) -> ServiceFuture<String>;
}

/// OrderInfos services, responsible for OrderInfo-related CRUD operations
pub struct OrderInfoServiceImpl<
    T: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
    M: ManageConnection<Connection = T>,
    F: ReposFactory<T>,
> {
    pub db_pool: Pool<M>,
    pub cpu_pool: CpuPool,
    pub http_client: ClientHandle,
    user_id: Option<UserId>,
    pub repo_factory: F,
    pub create_order_url: String,
    pub callback_url: String,
    pub saga_url: String,
}

impl<
        T: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
        M: ManageConnection<Connection = T>,
        F: ReposFactory<T>,
    > OrderInfoServiceImpl<T, M, F>
{
    pub fn new(
        db_pool: Pool<M>,
        cpu_pool: CpuPool,
        http_client: ClientHandle,
        user_id: Option<UserId>,
        repo_factory: F,
        create_order_url: String,
        callback_url: String,
        saga_url: String,
    ) -> Self {
        Self {
            db_pool,
            cpu_pool,
            http_client,
            user_id,
            repo_factory,
            create_order_url,
            callback_url,
            saga_url,
        }
    }
}

impl<
        T: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
        M: ManageConnection<Connection = T>,
        F: ReposFactory<T>,
    > OrderInfoService for OrderInfoServiceImpl<T, M, F>
{
    /// Creates orders in billing system, returning url for payment
    fn create_invoice(&self, create_order: CreateInvoice) -> ServiceFuture<URL> {
        let db_clone = self.db_pool.clone();
        let user_id = self.user_id;
        let repo_factory = self.repo_factory.clone();
        let client = self.http_client.clone();
        let external_billing_address = self.create_order_url.clone();
        let callback_url = self.callback_url.clone();

        Box::new(
            self.cpu_pool
                .spawn_fn(move || {
                    db_clone
                        .get()
                        .map_err(|e| e.context(Error::Connection).into())
                        .and_then(move |conn| {
                            let order_info_repo = repo_factory.create_order_info_repo(&conn, user_id);
                            let merchant_repo = repo_factory.create_merchant_repo(&conn, user_id);
                            let invoice_repo = repo_factory.create_invoice_repo(&conn, user_id);

                            conn.transaction::<URL, FailureError, _>(move || {
                                debug!("Creating new order_infos: {:?}", &create_order);
                                let callback_id = CallbackId::new();
                                create_order
                                    .orders
                                    .iter()
                                    .map(|order| {
                                        let payload = NewOrderInfo::new(order.id.clone(), callback_id.clone());
                                        order_info_repo.create(payload).and_then(|_| {
                                            merchant_repo
                                                .get_by_subject_id(SubjectIdentifier::Store(order.store_id.clone()))
                                                .map(|merchant| BillingOrder::new(order.clone(), merchant.merchant_id))
                                        })
                                    })
                                    .collect::<RepoResult<Vec<BillingOrder>>>()
                                    .and_then(|orders| {
                                        let callback = format!("{}/secret={}", callback_url, callback_id.0);
                                        let billing_payload =
                                            CreateInvoicePayload::new(orders, callback, create_order.currency_id.to_string());
                                        let body = serde_json::to_string(&billing_payload)?;
                                        let url = format!("{}", external_billing_address);
                                        client
                                            .request::<Invoice>(Post, url, Some(body), None)
                                            .map_err(|e| {
                                                e.context("Occured an error during invoice creation in external billing.")
                                                    .context(Error::HttpClient)
                                                    .into()
                                            })
                                            .wait()
                                    })
                                    .and_then(|invoice| {
                                        let payload = NewInvoice::new(invoice.id.clone(), invoice.billing_url.clone());
                                        invoice_repo.create(payload).map(|invoice| invoice.billing_url)
                                    })
                            })
                        })
                })
                .map_err(|e: FailureError| e.context("Service order_info, create endpoint error occured.").into()),
        )
    }

    /// Updates specific order_info
    fn set_paid(&self, callback_id: CallbackId) -> ServiceFuture<String> {
        let db_clone = self.db_pool.clone();
        let current_user = self.user_id;
        let client = self.http_client.clone();
        let repo_factory = self.repo_factory.clone();
        let saga_url = self.saga_url.clone();

        debug!("Seting order with callback id {:?} paid", &callback_id);

        Box::new(
            self.cpu_pool
                .spawn_fn(move || {
                    db_clone
                        .get()
                        .map_err(|e| e.context(Error::Connection).into())
                        .and_then(move |conn| {
                            let order_info_repo = repo_factory.create_order_info_repo(&conn, current_user);
                            order_info_repo.set_paid(callback_id)
                        })
                        .and_then(|orders| {
                            let body = serde_json::to_string(&orders)?;
                            let url = format!("{}/orders/set_paid", saga_url);
                            client
                                .request::<String>(Post, url, Some(body), None)
                                .map_err(|e| {
                                    e.context("Occured an error during setting orders paid in saga.")
                                        .context(Error::HttpClient)
                                        .into()
                                })
                                .wait()
                        })
                })
                .map_err(|e: FailureError| e.context("Service order_info, set_paid endpoint error occured.").into()),
        )
    }
}

#[cfg(test)]
pub mod tests {

    use std::sync::Arc;
    use tokio_core::reactor::Core;

    use stq_static_resources::Currency;

    use models::*;
    use repos::repo_factory::tests::*;
    use services::order_info::OrderInfoService;

    #[test]
    fn test_create_order_info() {
        let mut core = Core::new().unwrap();
        let handle = Arc::new(core.handle());
        let service = create_order_info_service(Some(UserId(1)), handle);
        let order = Order {
            id: OrderId::new(),
            store_id: StoreId(1),
            price: 3232.32,
            currency_id: CurrencyId(1),
        };
        let create_order = CreateInvoice {
            orders: vec![order],
            currency_id: CurrencyId(Currency::Stq as i32),
        };
        let work = service.create_invoice(create_order);
        let _result = core.run(work).unwrap();
    }

    #[test]
    fn test_set_paid() {
        let mut core = Core::new().unwrap();
        let handle = Arc::new(core.handle());
        let service = create_order_info_service(Some(UserId(1)), handle);
        let callback_id = CallbackId::new();
        let work = service.set_paid(callback_id);
        let _result = core.run(work).unwrap();
    }

}
