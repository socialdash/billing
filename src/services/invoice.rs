//! Invoices Services, presents CRUD operations with invoices
use std::str::FromStr;
use std::sync::Arc;

use bigdecimal::BigDecimal;
use chrono::{Duration, Utc};
use diesel::connection::AnsiTransactionManager;
use diesel::pg::Pg;
use diesel::Connection;
use failure::{err_msg, Error as FailureError, Fail};
use futures::{future, stream, Future, IntoFuture, Stream};
use hyper::header::{Authorization, Bearer, ContentType};
use hyper::Headers;
use hyper::Post;
use models::invoice_v2::InvoiceSetAmountPaid;
use models::invoice_v2::RawInvoice;
use r2d2::ManageConnection;
use secp256k1::{Message, PublicKey, Secp256k1, Signature};
use serde_json;
use sha2::digest::Digest;
use sha2::Sha256;
use uuid::Uuid;

use stq_http::client::HttpClient;
use stq_http::request_util::Sign as TureSignature;
use stq_types::stripe::PaymentIntentId;
use stq_types::{InvoiceId, OrderId, SagaId};

use client::payments::{GetRate, PaymentsClient, Rate, RateRefresh};
use client::stores::CurrencyExchangeInfo;
use client::stripe::{NewPaymentIntent as StripeClientNewPaymentIntent, StripeClient};
use config::ExternalBilling;
use controller::context::DynamicContext;
use errors::Error;
use models::invoice_v2::{calculate_invoice_price, InvoiceDump, InvoiceId as InvoiceV2Id, NewInvoice, RawInvoice as InvoiceV2};
use models::order_v2::{ExchangeId, NewOrder, OrderId as OrderV2Id, RawOrder};
use models::*;
use repos::error::ErrorKind as RepoErrorKind;
use repos::repo_factory::ReposFactory;
use repos::{
    AccountsRepo, EventStoreRepo, InvoicesV2Repo, OrderExchangeRatesRepo, OrdersRepo, PaymentIntentInvoiceRepo, PaymentIntentRepo,
    SearchPaymentIntentInvoice,
};
use services::accounts::AccountService;
use services::types::spawn_on_pool;
use services::Service;

use super::error::{Error as ServiceError, ErrorContext, ErrorKind};
use super::types::{ServiceFuture, ServiceFutureV2};

pub trait InvoiceService {
    /// Creates invoice in billing system
    fn create_invoice(&self, create_invoice: CreateInvoice) -> ServiceFuture<Invoice>;
    fn create_invoice_v2(&self, create_invoice: CreateInvoiceV2) -> ServiceFutureV2<InvoiceDump>;
    /// Get invoice by order id
    fn get_invoice_by_order_id(&self, order_id: OrderId) -> ServiceFuture<Option<Invoice>>;
    fn get_invoice_by_order_id_v1(&self, order_id: OrderId) -> ServiceFuture<Option<Invoice>>;
    fn get_invoice_by_order_id_v2(&self, order_id: OrderV2Id) -> ServiceFutureV2<Option<InvoiceDump>>;
    /// Get invoice by invoice id
    fn get_invoice_by_id(&self, id: InvoiceId) -> ServiceFuture<Option<Invoice>>;
    fn get_invoice_by_id_v1(&self, id: InvoiceId) -> ServiceFuture<Option<Invoice>>;
    /// Recalc invoice by invoice id
    /// Refreshes all rates for the invoice and calculates the total price of the invoice.
    /// Either calculate the current total price of the invoice or get the final price if the invoice has been paid
    fn recalc_invoice(&self, id: InvoiceId) -> ServiceFuture<Invoice>;
    fn recalc_invoice_v1(&self, id: InvoiceId) -> ServiceFuture<Invoice>;
    fn recalc_invoice_v2(&self, id: InvoiceV2Id) -> ServiceFutureV2<Option<InvoiceDump>>;
    /// Get orders ids by invoice id
    fn get_invoice_orders_ids(&self, id: InvoiceId) -> ServiceFuture<Vec<OrderId>>;
    fn get_invoice_orders_ids_v1(&self, id: InvoiceId) -> ServiceFuture<Vec<OrderId>>;
    fn get_invoice_orders_ids_v2(&self, id: InvoiceV2Id) -> ServiceFutureV2<Vec<OrderV2Id>>;
    /// Delete invoice
    fn delete_invoice_by_saga_id(&self, id: SagaId) -> ServiceFuture<SagaId>;
    fn delete_invoice_by_saga_id_v1(&self, id: SagaId) -> ServiceFuture<SagaId>;
    fn delete_invoice_by_saga_id_v2(&self, id: SagaId) -> ServiceFuture<SagaId>;
    /// DEPRECATED
    /// Creates orders in billing system, returning url for payment
    fn update_invoice(&self, invoice: ExternalBillingInvoice) -> ServiceFuture<()>;
    /// Handles the callback from Payments gateway which carries a new inbound transaction
    fn handle_inbound_tx(&self, signature_header: TureSignature, callback: PaymentsCallback, callback_body: String) -> ServiceFutureV2<()>;
    /// Get missing rates from Payments gateway and refresh existing rates
    fn get_missing_rates_from_payments_gateway_and_refresh_existing_rates(
        &self,
        invoice: InvoiceV2,
        current_order_rates: Vec<(RawOrder, Option<RawOrderExchangeRate>)>,
        user_id: Option<stq_types::UserId>,
    ) -> ServiceFutureV2<()>;
}

impl<
        T: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
        M: ManageConnection<Connection = T>,
        F: ReposFactory<T>,
        C: HttpClient + Clone,
        PC: PaymentsClient + Clone,
        AS: AccountService + Clone + 'static,
    > InvoiceService for Service<T, M, F, C, PC, AS>
{
    /// Creates orders in billing system, returning url for payment
    fn create_invoice(&self, create_invoice: CreateInvoice) -> ServiceFuture<Invoice> {
        if !self.payments_v2_enabled() {
            let e = err_msg("Could not create an invoice because Ture integration is not configured");
            Box::new(future::err(ectx!(err e => ErrorKind::Internal)))
        } else {
            let fut = CreateInvoiceV2::try_from_v1(create_invoice.clone())
                .map_err(ectx!(ErrorKind::Internal => create_invoice))
                .into_future()
                .and_then({
                    let self_ = self.clone();
                    move |create_invoice| self_.create_invoice_v2(create_invoice)
                })
                .and_then(|invoice_dump| {
                    invoice_dump
                        .clone()
                        .try_into_v1()
                        .map_err(ectx!(ErrorKind::Internal => invoice_dump))
                })
                .map_err(FailureError::from);

            Box::new(fut)
        }
    }

    fn create_invoice_v2(&self, create_invoice: CreateInvoiceV2) -> ServiceFutureV2<InvoiceDump> {
        let repo_factory = self.static_context.repo_factory.clone();
        let DynamicContext {
            user_id,
            payments_client,
            account_service,
            ..
        } = self.dynamic_context.clone();

        let (payments_client, account_service) = if let (Some(payments_client), Some(account_service)) = (payments_client, account_service)
        {
            (payments_client, account_service)
        } else {
            let e = err_msg("payments integration has not been configured");
            return Box::new(future::err::<_, ServiceError>(ectx!(err e, ErrorKind::Internal)));
        };

        let CreateInvoiceV2 {
            orders,
            customer_id: buyer_user_id,
            currency: buyer_currency,
            saga_id: invoice_id,
        } = create_invoice;

        let db_pool = self.static_context.db_pool.clone();
        let cpu_pool = self.static_context.cpu_pool.clone();

        let stripe_client = self.static_context.stripe_client.clone();

        let fut = stream::iter_ok::<_, ServiceError>(orders.into_iter().map(move |order| (payments_client.clone(), order)))
            .and_then(move |(payments_client, create_order)| {
                // process each order individually
                let CreateOrderV2 {
                    id,
                    store_id,
                    currency: seller_currency,
                    total_amount: seller_total_amount,
                    product_cashback: seller_cashback_percent,
                } = create_order;

                let total_amount = Amount::from_super_unit(seller_currency, BigDecimal::from(seller_total_amount));
                let cashback_amount = match seller_cashback_percent {
                    None => Amount::new(0),
                    Some(cashback_fraction) => Amount::from_super_unit(
                        seller_currency,
                        BigDecimal::from(seller_total_amount) * BigDecimal::from(cashback_fraction),
                    ),
                };

                let new_order = NewOrder {
                    id,
                    seller_currency,
                    total_amount,
                    cashback_amount,
                    invoice_id: invoice_id.clone(),
                    store_id,
                };

                match (buyer_currency.is_fiat(), seller_currency.is_fiat()) {
                    (true, true) => exchage_rate_fiat(new_order, buyer_currency, seller_currency),
                    (false, false) => exchage_rate_crypto(payments_client, new_order, buyer_currency, seller_currency, total_amount),
                    _ => {
                        let e = err_msg("fiat - crypto payments are not supported yet");
                        Box::new(future::err::<_, ServiceError>(ectx!(err e, ErrorKind::Internal)))
                    }
                }
            })
            .collect()
            .and_then(move |orders| {
                // process collection of orders
                if buyer_currency.is_fiat() {
                    future::Either::A(
                        create_payment_intent(stripe_client, &orders, invoice_id, buyer_currency)
                            .map(|new_payment_intent| (None, None, Some(new_payment_intent), orders)),
                    )
                } else {
                    future::Either::B(to_ture_currency(buyer_currency).and_then(move |buyer_currency| {
                        account_service
                            .get_or_create_free_pooled_account(buyer_currency)
                            .map_err(ectx!(convert => buyer_currency))
                            .map(|account| (Some(account.id), Some(account.wallet_address), None, orders))
                    }))
                }
            })
            .and_then({
                let payment_expiry = self.static_context.config.payment_expiry.clone();
                move |(account_id, wallet_address, new_payment_intent, orders)| {
                    cpu_pool.spawn_fn(move || {
                        db_pool.get().map_err(ectx!(ErrorKind::Internal)).and_then(move |conn| {
                            // Add scheduled PaymentExpired event
                            let payment_expired_event = Event::new(EventPayload::PaymentExpired { invoice_id });
                            let expiry_timeout = match new_payment_intent {
                                // use timeout crypto flow
                                None => Duration::minutes(payment_expiry.crypto_timeout_min as i64),
                                // use timeout for fiat flow
                                Some(_) => Duration::minutes(payment_expiry.fiat_timeout_min as i64),
                            };
                            let expires_on = Utc::now().naive_utc() + expiry_timeout;

                            let event_store_repo = repo_factory.create_event_store_repo_with_sys_acl(&conn);
                            event_store_repo
                                .add_scheduled_event(payment_expired_event.clone(), expires_on.clone())
                                .map_err(ectx!(try convert => payment_expired_event, expires_on))?;

                            // Save invoice data to database
                            let invoices_repo = repo_factory.create_invoices_v2_repo(&conn, user_id);
                            let orders_repo = repo_factory.create_orders_repo(&conn, user_id);
                            let order_exchange_rates_repo = repo_factory.create_order_exchange_rates_repo(&conn, user_id);
                            let payment_intent_repo = repo_factory.create_payment_intent_repo_with_sys_acl(&conn);
                            let payment_intent_invoices_repo = repo_factory.create_payment_intent_invoices_repo_with_sys_acl(&conn);

                            conn.transaction::<InvoiceDump, ServiceError, _>(move || {
                                let invoice = NewInvoice {
                                    id: invoice_id,
                                    account_id,
                                    buyer_currency,
                                    amount_captured: Amount::new(0u128),
                                    buyer_user_id,
                                };

                                let invoice = invoices_repo.create(invoice.clone()).map_err(ectx!(try convert => invoice))?;

                                if let Some((new_payment_intent, new_payment_intent_invoice)) = new_payment_intent {
                                    payment_intent_repo
                                        .create(new_payment_intent.clone())
                                        .map_err(ectx!(try convert => new_payment_intent))?;

                                    payment_intent_invoices_repo
                                        .create(new_payment_intent_invoice.clone())
                                        .map_err(ectx!(try convert => new_payment_intent_invoice))?;
                                }

                                let orders_with_rates = orders
                                    .into_iter()
                                    .map(|(new_order, exchange_id, exchange_rate)| {
                                        let order_id = new_order.id;

                                        let order = orders_repo.create(new_order.clone()).map_err(ectx!(try convert => new_order))?;

                                        let new_rate = NewOrderExchangeRate {
                                            order_id,
                                            exchange_id,
                                            exchange_rate,
                                        };

                                        let rate = order_exchange_rates_repo
                                            .add_new_active_rate(new_rate.clone())
                                            .map_err(ectx!(try convert => new_rate))?;

                                        Ok((order, vec![rate.active_rate]))
                                    })
                                    .collect::<Result<Vec<_>, ServiceError>>()?;

                                Ok(calculate_invoice_price(invoice, orders_with_rates, wallet_address))
                            })
                        })
                    })
                }
            });

        Box::new(fut)
    }

    /// Get invoice by order id

    fn get_invoice_by_order_id(&self, order_id: OrderId) -> ServiceFuture<Option<Invoice>> {
        let v2_handler = if self.payments_v2_enabled() {
            future::Either::A(
                self.get_invoice_by_order_id_v2(OrderV2Id::new(order_id.0))
                    .map_err(FailureError::from),
            )
        } else {
            future::Either::B(future::ok(None))
        };

        let fut =
            Future::join(self.get_invoice_by_order_id_v1(order_id), v2_handler).and_then(move |(invoice_v1, invoice_dump_v2)| {
                match (invoice_v1, invoice_dump_v2) {
                    (Some(_), Some(_)) => Err(format_err!("Order with ID: {} is stored both in v1 and v2 tables", order_id)),
                    (Some(invoice_v1), None) => Ok(Some(invoice_v1)),
                    (None, Some(invoice_dump_v2)) => invoice_dump_v2.clone().try_into_v1().map(Some).map_err(FailureError::from),
                    (None, None) => Ok(None),
                }
            });

        Box::new(fut)
    }

    fn get_invoice_by_order_id_v1(&self, order_id: OrderId) -> ServiceFuture<Option<Invoice>> {
        let user_id = self.dynamic_context.user_id;
        let repo_factory = self.static_context.repo_factory.clone();

        self.spawn_on_pool(move |conn| {
            let invoice_repo = repo_factory.create_invoice_repo(&conn, user_id);
            let order_info_repo = repo_factory.create_order_info_repo(&conn, user_id);
            debug!("Requesting invoice by order id: {}", &order_id);

            order_info_repo
                .find_by_order_id(order_id)
                .and_then(|order_info| {
                    if let Some(order_info) = order_info {
                        invoice_repo.find_by_saga_id(order_info.saga_id)
                    } else {
                        Ok(None)
                    }
                })
                .map_err(|e: FailureError| e.context("Service invoice, get_by_order_id endpoint error occured.").into())
        })
    }

    fn get_invoice_by_order_id_v2(&self, order_id: OrderV2Id) -> ServiceFutureV2<Option<InvoiceDump>> {
        let repo_factory = self.static_context.repo_factory.clone();
        let user_id = self.dynamic_context.user_id.clone();
        let db_pool = self.static_context.db_pool.clone();
        let cpu_pool = self.static_context.cpu_pool.clone();

        let fut = spawn_on_pool(db_pool, cpu_pool, move |conn| {
            let orders_repo = repo_factory.create_orders_repo(&conn, user_id);
            orders_repo.get(order_id.clone()).map_err(ectx!(convert => order_id))
        })
        .and_then({
            let self_ = self.clone();
            move |order| match order {
                None => future::Either::A(future::ok(None)),
                Some(order) => future::Either::B(future::lazy(move || {
                    self_.recalc_invoice_v2(order.invoice_id).and_then(move |invoice_dump| {
                        let e = format_err!(
                            "Invoice with ID: {} that is linked to order with ID: {} was not found",
                            order.invoice_id,
                            order.id,
                        );
                        invoice_dump.ok_or(ectx!(err e, ErrorKind::Internal)).map(Some)
                    })
                })),
            }
        });

        Box::new(fut)
    }

    /// Get invoice by invoice id

    fn get_invoice_by_id(&self, id: InvoiceId) -> ServiceFuture<Option<Invoice>> {
        let v2_handler = if self.payments_v2_enabled() {
            future::Either::A(self.recalc_invoice_v2(InvoiceV2Id::new(id.0)).map_err(FailureError::from))
        } else {
            future::Either::B(future::ok(None))
        };

        let fut = Future::join(self.get_invoice_by_id_v1(id), v2_handler).and_then(move |(invoice_v1, invoice_dump_v2)| {
            match (invoice_v1, invoice_dump_v2) {
                (Some(_), Some(_)) => Err(format_err!("Invoice with ID: {} is stored both in v1 and v2 tables", id)),
                (Some(invoice_v1), None) => Ok(Some(invoice_v1)),
                (None, Some(invoice_dump_v2)) => invoice_dump_v2.clone().try_into_v1().map(Some).map_err(FailureError::from),
                (None, None) => Ok(None),
            }
        });

        Box::new(fut)
    }

    fn get_invoice_by_id_v1(&self, id: InvoiceId) -> ServiceFuture<Option<Invoice>> {
        let repo_factory = self.static_context.repo_factory.clone();
        let user_id = self.dynamic_context.user_id;
        self.spawn_on_pool(move |conn| {
            let invoice_repo = repo_factory.create_invoice_repo(&conn, user_id);
            debug!("Requesting invoice by invoice id: {}", &id);
            invoice_repo
                .find(id)
                .map_err(|e: FailureError| e.context("Service invoice, get_by_id endpoint error occured.").into())
        })
    }

    /// Recalc invoice by invoice id

    fn recalc_invoice(&self, id: InvoiceId) -> ServiceFuture<Invoice> {
        let v2_handler = if self.payments_v2_enabled() {
            future::Either::A(self.recalc_invoice_v2(InvoiceV2Id::new(id.0)).map_err(FailureError::from))
        } else {
            future::Either::B(future::ok(None))
        };

        let fut = v2_handler.and_then({
            let self_ = self.clone();
            move |invoice_dump| match invoice_dump {
                None => future::Either::A(self_.recalc_invoice_v1(id)),
                Some(invoice_dump) => future::Either::B(invoice_dump.try_into_v1().map_err(FailureError::from).into_future()),
            }
        });

        Box::new(fut)
    }

    fn recalc_invoice_v1(&self, id: InvoiceId) -> ServiceFuture<Invoice> {
        let user_id = self.dynamic_context.user_id;
        let repo_factory = self.static_context.repo_factory.clone();
        let client = self.dynamic_context.http_client.clone();
        let ExternalBilling {
            invoice_url,
            login_url,
            username,
            password,
            ..
        } = self.static_context.config.external_billing.clone();
        let credentials = ExternalBillingCredentials::new(username, password);
        let saga_url = self.static_context.config.saga_addr.url.clone();

        self.spawn_on_pool(move |conn| {
            let invoice_repo = repo_factory.create_invoice_repo(&conn, user_id);
            let order_info_repo = repo_factory.create_order_info_repo(&conn, user_id);

            conn.transaction::<Invoice, FailureError, _>(move || {
                debug!("Recalculating invoice with id: {}", &id);
                let body = serde_json::to_string(&credentials)?;
                let url = login_url.to_string();
                let mut headers = Headers::new();
                headers.set(ContentType::json());
                client
                    .request_json::<ExternalBillingToken>(Post, url, Some(body), Some(headers))
                    .map_err(|e| {
                        e.context("Occured an error during receiving authorization token in external billing.")
                            .context(Error::HttpClient)
                            .into()
                    })
                    .and_then(|ext_token| {
                        let mut headers = Headers::new();
                        headers.set(Authorization(Bearer { token: ext_token.token }));
                        headers.set(ContentType::json());
                        let url = format!("{}{}/recalc/", invoice_url.to_string(), id);
                        client
                            .request_json::<ExternalBillingInvoice>(Post, url, None, Some(headers))
                            .map_err(|e| {
                                e.context("Occured an error during invoice recalculation in external billing.")
                                    .context(Error::HttpClient)
                                    .into()
                            })
                    })
                    .wait()
                    .and_then(|invoice| invoice_repo.update(id, invoice.into()))
                    .and_then(|invoice| {
                        order_info_repo
                            .update_status(invoice.id, invoice.state)
                            .and_then(|orders| {
                                let body = serde_json::to_string(&orders)?;
                                let url = format!("{}/orders/update_state", saga_url);
                                client
                                    .request_json::<()>(Post, url, Some(body), None)
                                    .map_err(|e| {
                                        e.context("Occured an error during setting orders new status in saga.")
                                            .context(Error::HttpClient)
                                            .into()
                                    })
                                    .wait()
                            })
                            .map(|_| invoice)
                    })
            })
            .map_err(|e: FailureError| e.context("Service invoice, recalc endpoint error occured.").into())
        })
    }

    fn recalc_invoice_v2(&self, id: InvoiceV2Id) -> ServiceFutureV2<Option<InvoiceDump>> {
        let db_pool = self.static_context.db_pool.clone();
        let cpu_pool = self.static_context.cpu_pool.clone();

        let fut = spawn_on_pool(db_pool, cpu_pool, {
            // Load invoice data (invoice, orders, active rates) for provided invoice ID

            let user_id = self.dynamic_context.user_id.clone();
            let repo_factory = self.static_context.repo_factory.clone();

            move |conn| {
                let invoices_repo = repo_factory.create_invoices_v2_repo(&conn, user_id);
                let orders_repo = repo_factory.create_orders_repo(&conn, user_id);
                let rates_repo = repo_factory.create_order_exchange_rates_repo(&conn, user_id);
                let accounts_repo = repo_factory.create_accounts_repo_with_sys_acl(&conn);

                let id_clone = id.clone();
                let invoice = invoices_repo.get(id_clone.clone()).map_err(ectx!(try convert => id_clone))?;

                let invoice = match invoice {
                    None => {
                        return Ok(None);
                    }
                    Some(invoice) => invoice,
                };

                let current_order_rates = get_order_active_rates(&*orders_repo, &*rates_repo, id)?;

                let wallet_address = if let Some(account_id) = invoice.account_id {
                    Some(
                        accounts_repo
                            .get(account_id.clone())
                            .map_err({
                                let account_id = account_id.clone();
                                ectx!(try convert => account_id)
                            })?
                            .ok_or({
                                let e = format_err!("Account {} not found", account_id);
                                ectx!(try err e, ErrorKind::Internal)
                            })?
                            .wallet_address,
                    )
                } else {
                    None
                };

                Ok(Some((invoice, current_order_rates, wallet_address)))
            }
        })
        .and_then({
            let db_pool = self.static_context.db_pool.clone();
            let cpu_pool = self.static_context.cpu_pool.clone();
            let repo_factory = self.static_context.repo_factory.clone();
            let user_id = self.dynamic_context.user_id;
            let self_ = self.clone();

            move |invoice_data| match invoice_data {
                None => future::Either::A(future::ok(None)),
                Some((invoice, current_order_rates, wallet_address)) => future::Either::B(Some(future::lazy(move || {
                    // Calculate invoice price without refreshing rates if the invoice has already been paid
                    if invoice.paid_at.is_some() {
                        let current_order_rates = current_order_rates
                            .into_iter()
                            .map(|(order, rate)| (order, rate.into_iter().collect::<Vec<_>>()))
                            .collect::<Vec<_>>();
                        return future::Either::A(future::ok(calculate_invoice_price(invoice, current_order_rates, wallet_address)));
                    }

                    // Get missing rates from Payments gateway and refresh existing rates
                    let fut = if invoice.buyer_currency.is_fiat() {
                        future::Either::A(future::ok(()))
                    } else {
                        future::Either::B(self_.get_missing_rates_from_payments_gateway_and_refresh_existing_rates(
                            invoice.clone(),
                            current_order_rates,
                            user_id,
                        ))
                    };

                    let fut = fut.and_then({
                        let db_pool = db_pool.clone();
                        let cpu_pool = cpu_pool.clone();
                        move |_| {
                            spawn_on_pool(db_pool, cpu_pool, move |conn| {
                                let invoices_repo = repo_factory.create_invoices_v2_repo(&conn, user_id);
                                let orders_repo = repo_factory.create_orders_repo(&conn, user_id);
                                let rates_repo = repo_factory.create_order_exchange_rates_repo(&conn, user_id);
                                let accounts_repo = repo_factory.create_accounts_repo_with_sys_acl(&conn);
                                let event_store_repo = repo_factory.create_event_store_repo_with_sys_acl(&conn);

                                calculate_invoice_price_and_set_final_price_if_paid(
                                    &*conn,
                                    &*invoices_repo,
                                    &*orders_repo,
                                    &*rates_repo,
                                    &*accounts_repo,
                                    &*event_store_repo,
                                    invoice.id.clone(),
                                )
                            })
                        }
                    });

                    future::Either::B(fut)
                }))),
            }
        });

        Box::new(fut)
    }

    /// Get orders ids by invoice id

    fn get_invoice_orders_ids(&self, id: InvoiceId) -> ServiceFuture<Vec<OrderId>> {
        let v2_handler = if self.payments_v2_enabled() {
            future::Either::A(self.get_invoice_orders_ids_v2(InvoiceV2Id::new(id.0)).map_err(FailureError::from))
        } else {
            future::Either::B(future::ok(vec![]))
        };

        let fut = Future::join(self.get_invoice_orders_ids_v1(id), v2_handler).and_then(move |(order_ids_v1, order_ids_v2)| {
            match (order_ids_v1.is_empty(), order_ids_v2.is_empty()) {
                (false, false) => Err(format_err!("Invoice with ID: {} is stored both in v1 and v2 tables", id)),
                (false, true) => Ok(order_ids_v1),
                (true, false) => Ok(order_ids_v2.into_iter().map(|id| OrderId(id.into_inner())).collect()),
                (true, true) => Ok(vec![]),
            }
        });

        Box::new(fut)
    }

    fn get_invoice_orders_ids_v1(&self, id: InvoiceId) -> ServiceFuture<Vec<OrderId>> {
        let user_id = self.dynamic_context.user_id;
        let repo_factory = self.static_context.repo_factory.clone();
        self.spawn_on_pool(move |conn| {
            let invoice_repo = repo_factory.create_invoice_repo(&conn, user_id);
            let order_info_repo = repo_factory.create_order_info_repo(&conn, user_id);
            debug!("Requesting vec order ids by invoice id: {}", &id);

            invoice_repo
                .find(id)
                .and_then(|invoice| {
                    if let Some(invoice) = invoice {
                        order_info_repo
                            .find_by_saga_id(invoice.id)
                            .map(|order_infos| order_infos.into_iter().map(|order_info| order_info.order_id).collect())
                    } else {
                        Ok(vec![])
                    }
                })
                .map_err(|e: FailureError| e.context("Service invoice, get_orders_ids endpoint error occured.").into())
        })
    }

    fn get_invoice_orders_ids_v2(&self, id: InvoiceV2Id) -> ServiceFutureV2<Vec<OrderV2Id>> {
        let db_pool = self.static_context.db_pool.clone();
        let cpu_pool = self.static_context.cpu_pool.clone();
        let repo_factory = self.static_context.repo_factory.clone();
        let user_id = self.dynamic_context.user_id;

        spawn_on_pool(db_pool, cpu_pool, move |conn| {
            let orders_repo = repo_factory.create_orders_repo(&conn, user_id);

            orders_repo
                .get_many_by_invoice_id(id.clone())
                .map(|orders| orders.into_iter().map(|order| order.id).collect())
                .map_err(ectx!(convert => id))
        })
    }

    /// Delete invoice
    fn delete_invoice_by_saga_id(&self, id: SagaId) -> ServiceFuture<SagaId> {
        if self.payments_v2_enabled() {
            self.delete_invoice_by_saga_id_v2(id)
        } else {
            self.delete_invoice_by_saga_id_v1(id)
        }
    }

    fn delete_invoice_by_saga_id_v1(&self, id: SagaId) -> ServiceFuture<SagaId> {
        let user_id = self.dynamic_context.user_id;
        let repo_factory = self.static_context.repo_factory.clone();

        self.spawn_on_pool(move |conn| {
            let invoice_repo = repo_factory.create_invoice_repo(&conn, user_id);
            let order_info_repo = repo_factory.create_order_info_repo(&conn, user_id);
            conn.transaction::<SagaId, FailureError, _>(move || {
                debug!("Deleting invoice: {}", &id);
                invoice_repo
                    .delete(id)
                    .and_then(|invoice| order_info_repo.delete_by_saga_id(invoice.id).map(|_| invoice.id))
            })
            .map_err(|e: FailureError| e.context("Service invoice, delete endpoint v1 error occured.").into())
        })
    }

    fn delete_invoice_by_saga_id_v2(&self, id: SagaId) -> ServiceFuture<SagaId> {
        let user_id = self.dynamic_context.user_id;
        let repo_factory = self.static_context.repo_factory.clone();
        let stripe_client = self.static_context.stripe_client.clone();

        let fut = self
            .spawn_on_pool(move |conn| {
                let invoices_repo = repo_factory.create_invoices_v2_repo(&conn, user_id);
                let orders_repo = repo_factory.create_orders_repo(&conn, user_id);
                let order_exchange_rates_repo = repo_factory.create_order_exchange_rates_repo(&conn, user_id);
                let payment_intent_repo = repo_factory.create_payment_intent_repo(&conn, user_id);
                let payment_intent_invoices_repo = repo_factory.create_payment_intent_invoices_repo_with_sys_acl(&conn);

                let invoice_id = InvoiceV2Id::new(id.0);
                conn.transaction::<_, FailureError, _>(move || {
                    debug!("Deleting invoice: {}", &id);
                    let deleted_orders = orders_repo.delete_by_invoice_id(invoice_id)?;

                    for order in deleted_orders {
                        order_exchange_rates_repo.delete_by_order_id(order.id)?;
                    }

                    let payment_intent_invoice = payment_intent_invoices_repo
                        .get(SearchPaymentIntentInvoice::InvoiceId(invoice_id))?
                        .ok_or({
                            let e = format_err!("Record payment_intent_invoice by invoice id {} not found", invoice_id);
                            ectx!(try err e, ErrorKind::Internal)
                        })?;

                    let deleted_payment_intent = payment_intent_repo.delete(payment_intent_invoice.payment_intent_id)?;

                    invoices_repo.delete(invoice_id)?;
                    Ok(deleted_payment_intent)
                })
                .map_err(|e: FailureError| e.context("Service invoice, delete endpoint v2 error occured.").into())
            })
            .and_then(move |deleted_payment_intent| {
                if let Some(deleted_payment_intent) = deleted_payment_intent {
                    future::Either::A(
                        stripe_client
                            .cancel_payment_intent(deleted_payment_intent.id)
                            .map_err(FailureError::from)
                            .map(|_| ()),
                    )
                } else {
                    future::Either::B(future::ok(()))
                }
            })
            .map(move |_| id);

        Box::new(fut)
    }

    /// DEPRECATED
    /// Updates specific invoice and orders
    fn update_invoice(&self, external_invoice: ExternalBillingInvoice) -> ServiceFuture<()> {
        let current_user = self.dynamic_context.user_id;
        let client = self.dynamic_context.http_client.clone();
        let repo_factory = self.static_context.repo_factory.clone();
        let saga_url = self.static_context.config.saga_addr.url.clone();

        debug!("Updating by external invoice {:?}.", &external_invoice);

        self.spawn_on_pool(move |conn| {
            let order_info_repo = repo_factory.create_order_info_repo(&conn, current_user);
            let invoice_repo = repo_factory.create_invoice_repo(&conn, current_user);
            let invoice_id = external_invoice.id;
            let update_payload = external_invoice.into();
            conn.transaction::<(), FailureError, _>(move || {
                invoice_repo
                    .update(invoice_id, update_payload)
                    .and_then(|invoice| order_info_repo.update_status(invoice.id, invoice.state))
                    .and_then(|orders| {
                        let body = serde_json::to_string(&orders)?;
                        let url = format!("{}/orders/update_state", saga_url);
                        client
                            .request_json::<()>(Post, url, Some(body), None)
                            .map_err(|e| {
                                e.context("Occured an error during setting orders new status in saga.")
                                    .context(Error::HttpClient)
                                    .into()
                            })
                            .wait()
                    })
            })
            .map_err(|e: FailureError| e.context("Service invoice, update endpoint error occured.").into())
        })
    }

    /// Handles the callback from Payments gateway which carries a new inbound transaction
    fn handle_inbound_tx(&self, signature_header: TureSignature, callback: PaymentsCallback, callback_body: String) -> ServiceFutureV2<()> {
        let payments_client = if let Some(payments_client) = self.dynamic_context.payments_client.clone() {
            payments_client
        } else {
            let e = err_msg("payments integration has not been configured");
            return Box::new(future::err::<_, ServiceError>(ectx!(err e, ErrorKind::Internal)));
        };

        let db_pool = self.static_context.db_pool.clone();
        let cpu_pool = self.static_context.cpu_pool.clone();
        let repo_factory = self.static_context.repo_factory.clone();

        let PaymentsCallback {
            transaction_id,
            account_id,
            amount_captured: amount_received,
            address: wallet_address,
            ..
        } = callback.clone();

        let signature_header = format!("{}", signature_header);
        let sign_public_key = if let Some(payments) = self.static_context.config.payments.clone() {
            payments.sign_public_key
        } else {
            let e = err_msg("sign public key not provided");
            return Box::new(future::err::<_, ServiceError>(ectx!(err e, ErrorKind::Internal)));
        };

        let fut =
            // Increase amount captured for the invoice
            spawn_on_pool(
                db_pool.clone(), cpu_pool.clone(),
                {
                    let repo_factory = repo_factory.clone();
                    move |conn| {
                        check_ture_sign(sign_public_key, signature_header, callback_body)?;
                        let invoices_repo = repo_factory.create_invoices_v2_repo_with_sys_acl(&conn);
                        let accounts_repo = repo_factory.create_accounts_repo_with_sys_acl(&conn);
                        let account_id = match account_id {
                            Some(account_id) => account_id,
                            None => accounts_repo.get_by_wallet_address(wallet_address.clone())
                                .map_err({let wallet_address = wallet_address.clone(); ectx!(try convert => wallet_address)})?
                                .ok_or_else(|| {
                                    let e = format_err!("Account with wallet address {} not found", wallet_address);
                                    ectx!(try err e, ErrorKind::NotFound)
                                })?
                                .id
                        };
                        let amount_received = Amount::from_str(&amount_received).map_err(move |e| {
                                let e = format_err!("Amount has wrong format: {}", e);
                                ectx!(try err e, ErrorKind::Internal => amount_received)
                            })?;

                        // if callback received to an account that is not connected to any invoice
                        let account_id_clone = account_id.clone();
                        if invoices_repo.get_by_account_id(account_id_clone.clone()).map_err(ectx!(try convert => account_id_clone))?.is_none() {
                            return Err(ErrorKind::NotFound.into());
                        }

                        invoices_repo.increase_amount_captured(account_id.clone(), transaction_id.clone(), amount_received)
                            .or_else(|e| match e.kind() {
                                // If the amount received has already been saved to the database, just get the invoice by account ID
                                RepoErrorKind::Constraints(_) => {
                                    invoices_repo.get_by_account_id(account_id.clone())
                                        .map_err({ let account_id = account_id.clone(); ectx!(convert => account_id) })
                                        .and_then(|invoice| invoice.ok_or_else(|| {
                                            let account_id = account_id.clone();
                                            let e = format_err!("Account with ID = {} is not linked to an invoice", account_id.clone());
                                            ectx!(err e, ErrorKind::Internal => account_id)
                                        }))
                                },
                                _ => Err(ectx!(convert err e => account_id, transaction_id, amount_received))
                            })
                    }
                }
            )
            // Recalc the total price of the invoice and set the final price if the amount captured >= total price
            .and_then({
                let db_pool = db_pool.clone();
                let cpu_pool = cpu_pool.clone();
                let repo_factory = repo_factory.clone();
                move |invoice| {
                    match invoice.paid_at.clone() {
                        // Do a recalc if the invoice is not paid
                        None => future::Either::A(future::lazy(move ||
                            spawn_on_pool(db_pool.clone(), cpu_pool.clone(), {
                                let invoice_id = invoice.id.clone();
                                let repo_factory = repo_factory.clone();
                                move |conn| {
                                    let orders_repo = repo_factory.create_orders_repo_with_sys_acl(&conn);
                                    let rates_repo = repo_factory.create_order_exchange_rates_repo_with_sys_acl(&conn);
                                    get_order_active_rates(&*orders_repo, &*rates_repo, invoice_id.clone())
                                }
                            })
                            // Get missing rates from Payments gateway and refresh existing rates
                            .and_then({
                                let buyer_currency = invoice.buyer_currency.clone();
                                move |current_order_rates| {
                                    to_ture_currency(buyer_currency.clone())
                                        .and_then(move |buyer_currency| refresh_rates(payments_client, buyer_currency, current_order_rates))
                                }
                            })
                            // Save new and updated rates to database
                            .and_then({
                                let db_pool = db_pool.clone();
                                let cpu_pool = cpu_pool.clone();
                                let repo_factory = repo_factory.clone();
                                move |new_active_rates| {
                                    spawn_on_pool(db_pool, cpu_pool, move |conn| {
                                        let rates_repo = repo_factory.create_order_exchange_rates_repo_with_sys_acl(&conn);

                                        new_active_rates
                                            .into_iter()
                                            .map(|new_rate| {
                                                rates_repo
                                                    .add_new_active_rate(new_rate.clone())
                                                    .map_err(ectx!(convert => new_rate))
                                                    .map(|_| ())
                                            })
                                            .collect::<Result<Vec<_>, ServiceError>>()
                                    })
                                }
                            })
                            .and_then({
                                let db_pool = db_pool.clone();
                                let cpu_pool = cpu_pool.clone();
                                let invoice = invoice.clone();
                                let repo_factory = repo_factory.clone();
                                move |_| spawn_on_pool(db_pool, cpu_pool, move |conn| {
                                    let invoices_repo = repo_factory.create_invoices_v2_repo_with_sys_acl(&conn);
                                    let orders_repo = repo_factory.create_orders_repo_with_sys_acl(&conn);
                                    let rates_repo = repo_factory.create_order_exchange_rates_repo_with_sys_acl(&conn);
                                    let accounts_repo = repo_factory.create_accounts_repo_with_sys_acl(&conn);
                                    let event_store_repo = repo_factory.create_event_store_repo_with_sys_acl(&conn);

                                    calculate_invoice_price_and_set_final_price_if_paid(
                                        &*conn,
                                        &*invoices_repo,
                                        &*orders_repo,
                                        &*rates_repo,
                                        &*accounts_repo,
                                        &*event_store_repo,
                                        invoice.id.clone(),
                                    )?;

                                    Ok(())
                                })
                            })
                        )),
                        // Skip recalc if the invoice is paid
                        Some(_) => future::Either::B(future::ok(())),
                    }
                }
            })
            .then(|res| {
                if let Err(e) = res {
                    match e.kind() {
                        ErrorKind::NotFound => Ok(()),
                        _ => Err(e)
                    }
                } else {
                    res
                }
            });

        Box::new(fut)
    }

    fn get_missing_rates_from_payments_gateway_and_refresh_existing_rates(
        &self,
        invoice: InvoiceV2,
        current_order_rates: Vec<(RawOrder, Option<RawOrderExchangeRate>)>,
        user_id: Option<stq_types::UserId>,
    ) -> ServiceFutureV2<()> {
        let db_pool = self.static_context.db_pool.clone();
        let cpu_pool = self.static_context.cpu_pool.clone();
        let repo_factory = self.static_context.repo_factory.clone();

        let fut = self
            .dynamic_context
            .payments_client
            .clone()
            .ok_or_else(|| {
                let e = err_msg("payments integration has not been configured");
                ectx!(err e, ErrorKind::Internal)
            })
            .into_future()
            .and_then(move |payments_client| {
                to_ture_currency(invoice.buyer_currency.clone()).map(move |buyer_currency| (payments_client, buyer_currency))
            })
            .and_then(move |(payments_client, buyer_currency)| refresh_rates(payments_client, buyer_currency, current_order_rates))
            // Save new and updated rates to database
            .and_then(move |new_active_rates| {
                spawn_on_pool(db_pool, cpu_pool, move |conn| {
                    let rates_repo = repo_factory.create_order_exchange_rates_repo(&conn, user_id);

                    new_active_rates
                        .into_iter()
                        .map(|new_rate| {
                            rates_repo
                                .add_new_active_rate(new_rate.clone())
                                .map_err(ectx!(convert => new_rate))
                                .map(|_| ())
                        })
                        .collect::<Result<Vec<_>, ServiceError>>()
                })
            })
            .map(|_| ());
        Box::new(fut)
    }
}

fn exchage_rate_fiat(
    new_order: NewOrder,
    buyer_currency: Currency,
    seller_currency: Currency,
) -> ServiceFutureV2<(NewOrder, Option<ExchangeId>, BigDecimal)> {
    //todo correct rates for fiat currencies
    if buyer_currency != seller_currency {
        let e = format_err!(
            "buyer currency ({}) and seller currency ({}) are not the same",
            buyer_currency,
            seller_currency
        );
        return Box::new(future::err(ectx!(err e, ErrorKind::Validation(serde_json::json!({
            "buyer_currency": buyer_currency,
            "seller_currency": seller_currency,
        })))));
    }
    Box::new(future::ok((new_order, None, BigDecimal::from(1))))
}

fn exchage_rate_crypto<PC>(
    payments_client: PC,
    new_order: NewOrder,
    buyer_currency: Currency,
    seller_currency: Currency,
    total_amount: Amount,
) -> ServiceFutureV2<(NewOrder, Option<ExchangeId>, BigDecimal)>
where
    PC: PaymentsClient + Send + Clone + 'static,
{
    let fut = Future::join(to_ture_currency(buyer_currency), to_ture_currency(seller_currency))
        .and_then(move |(buyer_currency, seller_currency)| get_rate(&payments_client, buyer_currency, seller_currency, total_amount))
        .map(|(exchange_id, exchange_rate)| (new_order, exchange_id, exchange_rate));

    Box::new(fut)
}

fn create_payment_intent(
    stripe_client: Arc<dyn StripeClient>,
    orders: &[(NewOrder, Option<ExchangeId>, BigDecimal)],
    invoice_id: InvoiceV2Id,
    buyer_currency: Currency,
) -> ServiceFutureV2<(NewPaymentIntent, NewPaymentIntentInvoice)> {
    let fut = payment_intent_create_params(orders, invoice_id, buyer_currency)
        .into_future()
        .and_then(move |payment_intent_creation| {
            stripe_client
                .create_payment_intent(payment_intent_creation)
                .map_err(ectx!(convert => invoice_id))
        })
        .and_then(move |stripe_payment_intent| new_payment_intent(invoice_id, stripe_payment_intent));

    Box::new(fut)
}

pub fn payment_intent_success<C>(
    conn: &C,
    orders_repo: &OrdersRepo,
    invoice_repo: &InvoicesV2Repo,
    _payment_intent_repo: &PaymentIntentRepo,
    payment_intent_invoices_repo: &PaymentIntentInvoiceRepo,
    payment_intent_id: PaymentIntentId,
) -> Result<(InvoiceV2, Vec<RawOrder>), ServiceError>
where
    C: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
{
    conn.transaction::<_, ServiceError, _>(move || {
        let payment_intent_id_cloned = payment_intent_id.clone();
        let payment_intent_invoice = payment_intent_invoices_repo
            .get(SearchPaymentIntentInvoice::PaymentIntentId(payment_intent_id.clone()))
            .map_err(ectx!(try convert => payment_intent_id_cloned))?
            .ok_or({
                let e = format_err!("Payment intent {} not found", payment_intent_id);
                ectx!(try err e, ErrorKind::Internal)
            })?;
        let invoice_id = payment_intent_invoice.invoice_id;
        let invoice = invoice_repo
            .get(invoice_id.clone())
            .map_err(ectx!(try convert => invoice_id.clone()))?
            .ok_or({
                let e = format_err!("Invoice {} not found", invoice_id.clone());
                ectx!(try err e, ErrorKind::Internal)
            })?;
        let orders = orders_repo
            .get_many_by_invoice_id(invoice.id)
            .map_err(ectx!(try convert => invoice_id))?;

        Ok((invoice, orders))
    })
}

pub fn get_rate<PC: PaymentsClient + Send + Clone + 'static>(
    payments_client: &PC,
    buyer_currency: TureCurrency,
    seller_currency: TureCurrency,
    total_amount: Amount,
) -> Box<Future<Item = (Option<ExchangeId>, BigDecimal), Error = ServiceError>> {
    Box::new(if buyer_currency == seller_currency {
        // Return dummy rate is the buyer pays with the same currency as seller
        future::Either::A(future::ok((None, BigDecimal::from(1))))
    } else {
        // Otherwise get the rate from Payments gateway

        let input = GetRate {
            id: Uuid::new_v4(),
            from: buyer_currency,
            to: seller_currency,
            amount_currency: seller_currency,
            amount: total_amount,
        };

        future::Either::B(
            payments_client
                .get_rate(input.clone())
                .map(|Rate { id, rate, .. }| (Some(ExchangeId::new(id)), rate))
                .map_err(ectx!(ErrorKind::Internal => input)),
        )
    })
}

pub fn get_order_active_rates(
    orders_repo: &OrdersRepo,
    rates_repo: &OrderExchangeRatesRepo,
    invoice_id: InvoiceV2Id,
) -> Result<Vec<(RawOrder, Option<RawOrderExchangeRate>)>, ServiceError> {
    orders_repo
        .get_many_by_invoice_id(invoice_id.clone())
        .map_err(ectx!(try convert => invoice_id))?
        .into_iter()
        .map(|order| {
            let order_id = order.id.clone();
            rates_repo
                .get_active_rate_for_order(order_id.clone())
                .map_err(ectx!(convert => order_id))
                .map(|rate| (order, rate))
        })
        .collect::<Result<Vec<_>, _>>()
}

/// Gets all of the invoice data by invoice ID from the DB and calculates the total price
pub fn get_invoice_price_by_invoice_id(
    invoices_repo: &InvoicesV2Repo,
    orders_repo: &OrdersRepo,
    rates_repo: &OrderExchangeRatesRepo,
    accounts_repo: &AccountsRepo,
    invoice_id: InvoiceV2Id,
) -> Result<Option<InvoiceDump>, ServiceError> {
    let invoice = invoices_repo.get(invoice_id.clone()).map_err(ectx!(try convert => invoice_id))?;

    match invoice {
        None => Ok(None),
        Some(invoice) => get_invoice_price(orders_repo, rates_repo, accounts_repo, invoice).map(Some),
    }
}

/// Gets all of the invoice data from the DB and calculates the total price
pub fn get_invoice_price(
    orders_repo: &OrdersRepo,
    rates_repo: &OrderExchangeRatesRepo,
    accounts_repo: &AccountsRepo,
    invoice: RawInvoice,
) -> Result<InvoiceDump, ServiceError> {
    let invoice_id = invoice.id.clone();
    let orders_with_rates = orders_repo
        .get_many_by_invoice_id(invoice_id.clone())
        .map_err(ectx!(try convert => invoice_id))?
        .into_iter()
        .map(|order| {
            let order_id = order.id.clone();
            rates_repo
                .get_all_rates_for_order(order_id.clone())
                .map_err(ectx!(convert => order_id))
                .map(|rates| (order, rates))
        })
        .collect::<Result<Vec<_>, ServiceError>>()?;

    let wallet_address = if let Some(account_id) = invoice.account_id {
        Some(
            accounts_repo
                .get(account_id.clone())
                .map_err({
                    let account_id = account_id.clone();
                    ectx!(try convert => account_id)
                })?
                .ok_or({
                    let e = format_err!("Account {} not found", account_id);
                    ectx!(try err e, ErrorKind::Internal)
                })?
                .wallet_address,
        )
    } else {
        None
    };

    Ok(calculate_invoice_price(invoice, orders_with_rates, wallet_address))
}

/// Returns new and updated active rates which then have to be saved in the database. Rates that remained the same get filetered out
pub fn refresh_rates<PC: PaymentsClient + Send + Clone + 'static>(
    payments_client: PC,
    buyer_currency: TureCurrency,
    current_order_rates: Vec<(RawOrder, Option<RawOrderExchangeRate>)>,
) -> Box<Future<Item = Vec<NewOrderExchangeRate>, Error = ServiceError>> {
    Box::new(
        stream::iter_ok(
            current_order_rates
                .into_iter()
                .map(move |(order, current_rate)| (payments_client.clone(), buyer_currency.clone(), order, current_rate)),
        )
        .and_then(|(pc, buyer_currency, order, current_rate)| reserve_or_refresh_rate(pc, buyer_currency, order, current_rate))
        .filter_map(|x| x)
        .collect(),
    )
}

/// Gets or refreshes an exchange rate. If the rate remains the same the function will return `None`
pub fn reserve_or_refresh_rate<PC: PaymentsClient + Send + Clone + 'static>(
    payments_client: PC,
    buyer_currency: TureCurrency,
    order: RawOrder,
    current_rate: Option<RawOrderExchangeRate>,
) -> Box<Future<Item = Option<NewOrderExchangeRate>, Error = ServiceError>> {
    let RawOrder {
        id: order_id,
        seller_currency,
        total_amount,
        ..
    } = order;
    let fut = match current_rate {
        // If the current rate wasn't provided, reserve a new rate though Payments API
        None => future::Either::A(to_ture_currency(seller_currency.clone()).and_then(move |seller_currency| {
            get_rate(&payments_client, buyer_currency, seller_currency, total_amount).map(move |(exchange_id, exchange_rate)| {
                Some(NewOrderExchangeRate {
                    order_id,
                    exchange_id,
                    exchange_rate,
                })
            })
        })),
        Some(RawOrderExchangeRate { exchange_id, .. }) => future::Either::B(match exchange_id {
            // If the current rate didn't have an exchange ID, which means that it's a dummy rate (1.0), then leave it be
            None => future::Either::A(future::ok(None)),
            // If the current rate has an exchange ID, refresh it through Payments API
            Some(id) => future::Either::B(future::lazy(move || {
                payments_client
                    .refresh_rate(id.clone())
                    .map_err(ectx!(convert ErrorKind::Internal => exchange_id))
                    .map(move |RateRefresh { rate, is_new_rate }| {
                        // If we got an updated rate from Payments API, return it
                        if is_new_rate {
                            let Rate {
                                id, rate: exchange_rate, ..
                            } = rate;
                            Some(NewOrderExchangeRate {
                                order_id,
                                exchange_id: Some(ExchangeId::new(id)),
                                exchange_rate,
                            })
                        // Otherwise, the rate remained unchanged so we don't create a new one
                        } else {
                            None
                        }
                    })
            })),
        }),
    };
    Box::new(fut)
}

pub fn calculate_invoice_price_and_set_final_price_if_paid<C>(
    conn: &C,
    invoices_repo: &InvoicesV2Repo,
    orders_repo: &OrdersRepo,
    rates_repo: &OrderExchangeRatesRepo,
    accounts_repo: &AccountsRepo,
    event_store_repo: &EventStoreRepo,
    invoice_id: InvoiceV2Id,
) -> Result<InvoiceDump, ServiceError>
where
    C: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
{
    conn.transaction::<_, ServiceError, _>(move || {
        let invoice = invoices_repo
            .get(invoice_id.clone())
            .map_err(ectx!(try convert => invoice_id))?
            .ok_or_else(|| {
                let e = format_err!("Invoice with ID {} does not exist", invoice_id);
                ectx!(try err e, ErrorKind::Internal => invoice_id)
            })?;

        let invoice_dump = get_invoice_price(&*orders_repo, &*rates_repo, &*accounts_repo, invoice.clone())?;

        // Do not update anything in DB if the invoice is already marked as paid
        if invoice.paid_at.is_some() {
            Ok(invoice_dump)
        } else {
            let has_become_paid = !invoice_dump.has_missing_rates
                && invoice.amount_captured.clone().to_super_unit(invoice_dump.buyer_currency.clone()) >= invoice_dump.total_price;
            // If the invoice became paid, save the total values and mark is as paid in the DB
            if !has_become_paid {
                Ok(invoice_dump)
            } else {
                let input = InvoiceSetAmountPaid {
                    final_amount_paid: Amount::from_super_unit(invoice_dump.buyer_currency.clone(), invoice_dump.total_price.clone()),
                    final_cashback_amount: Amount::from_super_unit(
                        Currency::Stq,
                        invoice_dump.total_cashback.clone().unwrap_or(BigDecimal::from(0)),
                    ),
                    paid_at: chrono::Utc::now().naive_utc(),
                };

                let invoice_id = invoice.id.clone();
                let invoice_dump = invoices_repo
                    .set_amount_paid(invoice_id.clone(), input.clone())
                    .map_err(ectx!(try convert => invoice_id, input))
                    .map(|_| invoice_dump)?;

                // Publish "InvoicePaid" event
                let event = Event::new(EventPayload::InvoicePaid { invoice_id: invoice.id });
                event_store_repo.add_event(event.clone()).map_err(ectx!(try convert => event))?;

                Ok(invoice_dump)
            }
        }
    })
}

fn payment_intent_create_params(
    orders: &[(NewOrder, Option<ExchangeId>, BigDecimal)],
    invoice_id: InvoiceV2Id,
    buyer_currency: Currency,
) -> Result<StripeClientNewPaymentIntent, ServiceError> {
    use bigdecimal::ToPrimitive;

    let exchanged_amount: BigDecimal = orders
        .iter()
        .map(|(order, _, exchange_rate)| {
            let seller_price: BigDecimal = order.total_amount.into();
            let exchanged_price = seller_price / exchange_rate;
            exchanged_price
        })
        .fold(BigDecimal::from(0), |acc, next| acc + next);
    let amount = exchanged_amount.to_u64().ok_or_else(|| {
        let e = format_err!("Invoice with ID: {} can not convert total_price: {}", invoice_id, exchanged_amount,);
        ectx!(try err e, ErrorKind::Internal)
    })?;

    Ok(StripeClientNewPaymentIntent {
        allowed_source_types: vec![stripe::PaymentIntentSourceType::Card],
        amount,
        currency: buyer_currency.try_into_stripe_currency().map_err(|_| {
            let e = format_err!("Invoice with ID: {} can not convert total_price: {}", invoice_id, buyer_currency,);
            ectx!(try err e, ErrorKind::Internal)
        })?,
        capture_method: Some(stripe::CaptureMethod::Automatic),
    })
}

fn new_payment_intent(
    invoice_id: InvoiceV2Id,
    stripe_payment_intent: stripe::PaymentIntent,
) -> Result<(NewPaymentIntent, NewPaymentIntentInvoice), ServiceError> {
    let payment_intent = NewPaymentIntent {
        id: PaymentIntentId(stripe_payment_intent.id.clone()),
        amount: stripe_payment_intent.amount.into(),
        amount_received: stripe_payment_intent.amount_received.into(),
        client_secret: stripe_payment_intent.client_secret,
        currency: Currency::try_from_stripe_currency(stripe_payment_intent.currency).map_err({
            let e = format_err!(
                "Payment intent for invoice with ID: {} can not convert currency: {}",
                invoice_id,
                stripe_payment_intent.currency,
            );
            move |_| ectx!(try err e, ErrorKind::Internal)
        })?,
        last_payment_error_message: stripe_payment_intent.last_payment_error.map(|err| format!("{:?}", err)),
        receipt_email: stripe_payment_intent.receipt_email,
        charge_id: stripe_payment_intent
            .charges
            .data
            .into_iter()
            .next()
            .map(|charge| ChargeId::new(charge.id)),
        status: stripe_payment_intent.status.into(),
    };

    let payment_intent_invoice = NewPaymentIntentInvoice {
        invoice_id,
        payment_intent_id: PaymentIntentId(stripe_payment_intent.id),
    };

    Ok((payment_intent, payment_intent_invoice))
}

pub fn to_ture_currency(currency: Currency) -> Box<Future<Item = TureCurrency, Error = ServiceError>> {
    Box::new(
        TureCurrency::try_from_currency(currency.clone())
            .map_err({
                let e = format_err!("Unsupported currency: {}", currency);
                |_| ectx!(err e, ErrorKind::Internal)
            })
            .into_future(),
    )
}

pub fn check_ture_sign(sign_public_key: String, signature: String, body: String) -> Result<(), ServiceError> {
    let mut hasher = Sha256::new();
    hasher.input(&body);
    let bytes = hasher.result();
    let message = Message::from_slice(&bytes).map_err(ectx!(try ErrorContext::WrongMessage, ErrorKind::Forbidden))?;
    let secp = Secp256k1::new();
    let public_key =
        PublicKey::from_slice(&parse_hex(&sign_public_key)).map_err(ectx!(try ErrorContext::PublicKey, ErrorKind::Forbidden))?;
    let sig = Signature::from_compact(&parse_hex(&signature)).map_err(ectx!(try ErrorContext::Sign, ErrorKind::Forbidden))?;
    secp.verify(&message, &sig, &public_key)
        .map_err(ectx!(ErrorContext::VerifySign, ErrorKind::Forbidden))
}

pub fn parse_hex(hex_asm: &str) -> Vec<u8> {
    let mut hex_bytes = hex_asm
        .as_bytes()
        .iter()
        .filter_map(|b| match b {
            b'0'...b'9' => Some(b - b'0'),
            b'a'...b'f' => Some(b - b'a' + 10),
            b'A'...b'F' => Some(b - b'A' + 10),
            _ => None,
        })
        .fuse();

    let mut bytes = Vec::new();
    while let (Some(h), Some(l)) = (hex_bytes.next(), hex_bytes.next()) {
        bytes.push(h << 4 | l)
    }
    bytes
}

/// The Commission for the services of the platform from sellers who trade in ' STQ ' is deducted in Fiat currency.
/// Conversion rates from` Crypto `to` Fiat `are stored per 1` STQ',
/// and the order stores the amount in cents, so the conversion from cents and back is used.
pub fn create_crypto_fee(
    order_percent: u64,
    fee_currency: &Currency,
    currency_exchange_info: &CurrencyExchangeInfo,
    order: &RawOrder,
) -> Result<NewFee, ServiceError> {
    let hundred_percents = 100u64;

    let exchange_rate = currency_exchange_info
        .data
        .get(&order.seller_currency)
        .and_then(|exchanges| exchanges.get(&fee_currency).map(|c| c.0))
        .ok_or(ectx!(try err ErrorContext::AmountConversion, ErrorKind::Internal))?;

    let total_amount_super_unit = order.total_amount.to_super_unit(order.seller_currency);
    let convert_total_amount = Amount::from_super_unit(fee_currency.clone(), total_amount_super_unit / BigDecimal::from(exchange_rate));

    let amount = convert_total_amount
        .checked_div(Amount::from(hundred_percents))
        .and_then(|one_percent| one_percent.checked_mul(Amount::from(order_percent)))
        .ok_or(ectx!(try err ErrorContext::AmountConversion, ErrorKind::Internal))?;

    Ok(NewFee {
        order_id: order.id,
        amount,
        status: FeeStatus::NotPaid,
        currency: *fee_currency,
        charge_id: None,
        metadata: None,
        crypto_currency: Some(order.seller_currency.clone()),
        crypto_amount: Some(order.total_amount.clone()),
    })
}

#[cfg(test)]
pub mod tests {

    use bigdecimal::BigDecimal;
    use chrono::NaiveDateTime;
    use std::sync::Arc;
    use std::time::SystemTime;
    use tokio_core::reactor::Core;
    use uuid::Uuid;

    use models::currency::Currency as StqCurrency;
    use stq_static_resources::Currency;
    use stq_types::*;

    use client::stores::*;
    use models::invoice_v2::InvoiceId as InvoiceIdv2;
    use models::order_v2::{OrderId as OrderIdv2, RawOrder, StoreId as StoreIdv2};
    use models::*;
    use repos::repo_factory::tests::*;
    use services::invoice::create_crypto_fee;
    use services::invoice::InvoiceService;
    use services::merchant::MerchantService;

    #[test]
    #[ignore]
    fn test_create_order_info() {
        let id = UserId(1);
        let mut core = Core::new().unwrap();
        let handle = Arc::new(core.handle());
        let service = create_service(Some(id), handle);

        let create_user = CreateUserMerchantPayload { id };
        let work = service.create_user(create_user);
        let _merchant = core.run(work).unwrap();

        let create_store = CreateStoreMerchantPayload {
            id: StoreId(1),
            country_code: None,
        };
        let work = service.create_store(create_store);
        let _store_merchant = core.run(work).unwrap();

        let order = Order {
            id: OrderId::new(),
            store_id: StoreId(1),
            price: ProductPrice(3232.32),
            quantity: Quantity(1),
            currency: Currency::STQ,
            total_amount: ProductPrice(3232.32),
            product_cashback: None,
        };

        let create_order = CreateInvoice {
            saga_id: SagaId::new(),
            customer_id: UserId(1),
            orders: vec![order],
            currency: Currency::STQ,
        };
        let work = service.create_invoice(create_order);
        let _result = core.run(work).unwrap();
    }

    #[test]
    #[ignore]
    fn test_set_paid() {
        let mut core = Core::new().unwrap();
        let handle = Arc::new(core.handle());
        let service = create_service(Some(UserId(1)), handle);
        let invoice = ExternalBillingInvoice {
            id: InvoiceId::new(),
            amount: "0.000000000".to_string(),
            status: ExternalBillingStatus::New,
            wallet: Some("wallet".to_string()),
            amount_captured: "0.000000000".to_string(),
            transactions: None,
            currency: Currency::STQ,
            expired: SystemTime::now().into(),
        };
        let work = service.update_invoice(invoice);
        let _result = core.run(work).unwrap();
    }

    #[test]
    fn check_conversion_currencies() {
        // given
        let order_percent = 5;
        let fee_currency = StqCurrency::Eur;
        let crypto_currency = StqCurrency::Stq;

        let mut data = CurrencyExchangeData::new();
        let mut exchange_rates = ExchangeRates::new();

        exchange_rates.insert(fee_currency, ExchangeRate(5.0));
        data.insert(crypto_currency, exchange_rates);

        let currency_exchange_info = CurrencyExchangeInfo {
            id: CurrencyExchangeId(Uuid::new_v4()),
            data,
        };

        let order = RawOrder {
            id: OrderIdv2::new(Uuid::new_v4()),
            seller_currency: crypto_currency,
            total_amount: Amount::from_super_unit(crypto_currency, BigDecimal::from(100)),
            cashback_amount: Amount::new(0),
            invoice_id: InvoiceIdv2::new(Uuid::new_v4()),
            created_at: NaiveDateTime::from_timestamp(0, 0),
            updated_at: NaiveDateTime::from_timestamp(0, 0),
            store_id: StoreIdv2::new(1),
            state: PaymentState::Initial,
            stripe_fee: None,
        };

        // then
        let new_fee = create_crypto_fee(order_percent, &fee_currency, &currency_exchange_info, &order).expect("cannot get new fee");

        assert_eq!(new_fee.amount, Amount::from_super_unit(fee_currency, BigDecimal::from(1)));
    }

}
