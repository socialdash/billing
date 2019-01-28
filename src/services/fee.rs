//! FeesService Services, presents CRUD operations with fee table
use std::collections::HashMap;
use std::sync::Arc;

use diesel::connection::AnsiTransactionManager;
use diesel::pg::Pg;
use diesel::Connection;
use futures_cpupool::CpuPool;
use r2d2::{ManageConnection, Pool};

use failure::Fail;

use futures::Future;
use stq_http::client::HttpClient;
use stq_types::StoreId as StqStoreId;

use client::payments::PaymentsClient;
use client::stripe::{NewCharge, StripeClient};
use services::accounts::AccountService;

use models::{fee::FeeId, order_v2::OrderId, ChargeId, FeeStatus, SubjectIdentifier, UpdateFee};
use repos::{ReposFactory, SearchCustomer, SearchFee};

use super::types::ServiceFutureV2;
use controller::{context::DynamicContext, responses::FeeResponse};
use services::ErrorKind;

use services::types::spawn_on_pool;

pub trait FeesService {
    /// Getting fee by order id
    fn get_by_order_id(&self, order_id: OrderId) -> ServiceFutureV2<Option<FeeResponse>>;
    /// Create Charge object in Stripe
    fn create_charge(&self, id_arg: FeeId) -> ServiceFutureV2<FeeResponse>;
}

pub struct FeesServiceImpl<
    T: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
    M: ManageConnection<Connection = T>,
    F: ReposFactory<T>,
    C: HttpClient + Clone,
    PC: PaymentsClient + Clone,
    AS: AccountService + Clone,
> {
    pub db_pool: Pool<M>,
    pub cpu_pool: CpuPool,
    pub repo_factory: F,
    pub stripe_client: Arc<dyn StripeClient>,
    pub dynamic_context: DynamicContext<C, PC, AS>,
}

impl<
        T: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
        M: ManageConnection<Connection = T>,
        F: ReposFactory<T>,
        C: HttpClient + Clone,
        PC: PaymentsClient + Clone,
        AS: AccountService + Clone,
    > FeesService for FeesServiceImpl<T, M, F, C, PC, AS>
{
    fn get_by_order_id(&self, order_id: OrderId) -> ServiceFutureV2<Option<FeeResponse>> {
        debug!("Requesting fee record by order id: {}", order_id);

        let repo_factory = self.repo_factory.clone();
        let user_id = self.dynamic_context.user_id;
        let db_pool = self.db_pool.clone();
        let cpu_pool = self.cpu_pool.clone();

        spawn_on_pool(db_pool, cpu_pool, move |conn| {
            let fees_repo = repo_factory.create_fees_repo(&conn, user_id);

            fees_repo
                .get(SearchFee::OrderId(order_id))
                .map_err(ectx!(convert => order_id))
                .and_then(|fee| {
                    if let Some(fee) = fee {
                        FeeResponse::try_from_fee(fee).map(|res| Some(res))
                    } else {
                        Ok(None)
                    }
                })
        })
    }

    fn create_charge(&self, id_arg: FeeId) -> ServiceFutureV2<FeeResponse> {
        debug!("Create charge in stripe by fee id: {}", id_arg);

        let repo_factory = self.repo_factory.clone();
        let repo_factory2 = self.repo_factory.clone();
        let user_id = self.dynamic_context.user_id;
        let db_pool = self.db_pool.clone();
        let cpu_pool = self.cpu_pool.clone();
        let db_pool2 = self.db_pool.clone();
        let cpu_pool2 = self.cpu_pool.clone();
        let stripe_client = self.stripe_client.clone();

        let fut = spawn_on_pool(db_pool, cpu_pool, move |conn| {
            let fees_repo = repo_factory.create_fees_repo(&conn, user_id);
            let merchant_repo = repo_factory.create_merchant_repo(&conn, user_id);
            let order_repo = repo_factory.create_orders_repo(&conn, user_id);
            let customers_repo = repo_factory.create_customers_repo(&conn, user_id);

            let current_fee = fees_repo.get(SearchFee::Id(id_arg)).map_err(ectx!(try convert => id_arg))?.ok_or({
                let e = format_err!("Fee by id {} not found", id_arg);
                ectx!(try err e, ErrorKind::Internal)
            })?;

            let order_id_cloned = current_fee.order_id.clone();
            let current_order = order_repo
                .get(current_fee.order_id)
                .map_err(ectx!(try convert => order_id_cloned))?
                .ok_or({
                    let e = format_err!("Order by id {} not found", current_fee.order_id);
                    ectx!(try err e, ErrorKind::Internal)
                })?;

            let store_id_cloned = current_order.store_id;
            let current_merchant = merchant_repo
                .get_by_subject_id(SubjectIdentifier::Store(StqStoreId(current_order.store_id.inner())))
                .map_err(|e| ectx!(try err e, ErrorKind::Internal => store_id_cloned))?;

            let merchant_owner = current_merchant.user_id.ok_or({
                let e = format_err!("Merchant owner by store id {} not found", current_order.store_id);
                ectx!(try err e, ErrorKind::Internal)
            })?;

            let merchant_owner_cloned = merchant_owner.clone();
            let stripe_customer = customers_repo
                .get(SearchCustomer::UserId(merchant_owner))
                .map_err(ectx!(try convert => merchant_owner_cloned))?
                .ok_or({
                    let e = format_err!("Customer by user id {} not found", merchant_owner);
                    ectx!(try err e, ErrorKind::Internal)
                })?;

            Ok((current_fee, stripe_customer))
        })
        .and_then(move |(fee, customer)| {
            let new_charge = NewCharge {
                customer_id: customer.id.clone(),
                amount: fee.amount,
                currency: fee.currency,
                capture: true,
            };

            let customer_id_cloned = customer.id.clone();
            let mut metadata = HashMap::new();
            metadata.insert("order_id".to_string(), format!("{}", fee.order_id));
            metadata.insert("fee_id".to_string(), format!("{}", fee.id));

            stripe_client
                .create_charge(new_charge, Some(metadata))
                .map_err(ectx!(convert => customer_id_cloned))
                .map(|charge| (fee, charge))
        })
        .and_then(move |(fee, charge)| {
            spawn_on_pool(db_pool2, cpu_pool2, move |conn| {
                let fees_repo = repo_factory2.create_fees_repo(&conn, user_id);

                let status = if charge.paid {
                    Some(FeeStatus::Paid)
                } else {
                    Some(FeeStatus::Fail)
                };
                let charge_id = Some(charge.id).map(|v| ChargeId::new(v));
                let update_fee = UpdateFee {
                    charge_id,
                    status,
                    ..Default::default()
                };

                let fee_id_cloned = fee.id.clone();
                fees_repo
                    .update(fee.id, update_fee)
                    .map_err(ectx!(convert => fee_id_cloned))
                    .and_then(|res| FeeResponse::try_from_fee(res))
            })
        });

        Box::new(fut)
    }
}
