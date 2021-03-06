use chrono::Duration;
use diesel::connection::AnsiTransactionManager;
use diesel::pg::Pg;
use diesel::Connection;
use future::Future;
use futures_cpupool::CpuPool;
use r2d2::{ManageConnection, Pool};
use uuid::Uuid;

use failure::Fail;

use stq_http::client::HttpClient;
use stq_types::StoreId;

use super::types::ServiceFutureV2;
use client::payments::PaymentsClient;
use config::Subscription as SubscriptionConfig;
use controller::context::DynamicContext;
use controller::requests::{CreateStoreSubscriptionRequest, UpdateStoreSubscriptionRequest};
use controller::responses::StoreSubscriptionResponse;
use models::{
    Amount, CreateStoreSubscription, Currency, NewStoreSubscription, StoreSubscriptionSearch, TureCurrency, UpdateStoreSubscription,
};
use repos::repo_factory::ReposFactory;
use services::accounts::AccountService;
use services::subscription::DEFAULT_EUR_CENTS_AMOUNT;
use services::subscription::DEFAULT_STQ_WEI_AMOUNT;
use services::types::spawn_on_pool;
use services::ErrorKind;

pub trait StoreSubscriptionService {
    fn create(&self, store_id: StoreId, payload: CreateStoreSubscriptionRequest) -> ServiceFutureV2<StoreSubscriptionResponse>;
    fn get(&self, store_id: StoreId) -> ServiceFutureV2<Option<StoreSubscriptionResponse>>;
    fn update(&self, store_id: StoreId, payload: UpdateStoreSubscriptionRequest) -> ServiceFutureV2<StoreSubscriptionResponse>;
}

pub struct StoreSubscriptionServiceImpl<
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
    pub dynamic_context: DynamicContext<C, PC, AS>,
    pub config: SubscriptionConfig,
}

impl<
        T: Connection<Backend = Pg, TransactionManager = AnsiTransactionManager> + 'static,
        M: ManageConnection<Connection = T>,
        F: ReposFactory<T>,
        C: HttpClient + Clone,
        PC: PaymentsClient + Clone,
        AS: AccountService + Clone,
    > StoreSubscriptionService for StoreSubscriptionServiceImpl<T, M, F, C, PC, AS>
{
    fn create(&self, store_id: StoreId, payload: CreateStoreSubscriptionRequest) -> ServiceFutureV2<StoreSubscriptionResponse> {
        let repo_factory = self.repo_factory.clone();
        let user_id = self.dynamic_context.user_id;

        let db_pool = self.db_pool.clone();
        let cpu_pool = self.cpu_pool.clone();

        let payload: CreateStoreSubscription = payload.into();

        let max_trial_duration = Duration::days(self.config.trial_time_duration_days);

        let account_service = match self.dynamic_context.account_service.clone() {
            Some(account_service) => account_service,
            None => {
                let e = format_err!("Accounts service was not found in dynamic context");
                return Box::new(futures::future::err(ectx!(err e, ErrorKind::Internal))) as ServiceFutureV2<StoreSubscriptionResponse>;
            }
        };

        let fut = match payload.currency {
            Currency::Eur => Box::new(futures::future::ok(NewStoreSubscription {
                store_id,
                currency: payload.currency,
                value: Amount::new(DEFAULT_EUR_CENTS_AMOUNT),
                wallet_address: None,
                trial_start_date: None,
            })),
            Currency::Stq => create_store_subscription_account(account_service, store_id),
            Currency::Eth | Currency::Btc | Currency::Usd | Currency::Rub => {
                let e = format_err!("Only {} and {} is allowed", Currency::Stq, Currency::Eur);
                return Box::new(futures::future::err(ectx!(err e, ErrorKind::Validation(serde_json::json!({
                    "currency": payload.currency,
                })))));
            }
        }
        .and_then(move |new_store_subscription| {
            spawn_on_pool(db_pool, cpu_pool, move |conn| {
                let store_subscription_repo = repo_factory.create_store_subscription_repo(&conn, user_id);

                let result = store_subscription_repo.create(new_store_subscription).map_err(ectx!(try convert))?;

                Ok(StoreSubscriptionResponse {
                    store_id: result.store_id,
                    currency: result.currency.into(),
                    value: result.value.to_super_unit(result.currency),
                    wallet_address: result.wallet_address,
                    trial_start_date: result.trial_start_date,
                    trial_end_date: result.trial_start_date.map(|date| date + max_trial_duration),
                    created_at: result.created_at,
                    updated_at: result.updated_at,
                    status: result.status,
                })
            })
        });

        Box::new(fut)
    }

    fn get(&self, store_id: StoreId) -> ServiceFutureV2<Option<StoreSubscriptionResponse>> {
        let repo_factory = self.repo_factory.clone();
        let user_id = self.dynamic_context.user_id;

        let db_pool = self.db_pool.clone();
        let cpu_pool = self.cpu_pool.clone();

        let max_trial_duration = Duration::days(self.config.trial_time_duration_days);

        spawn_on_pool(db_pool, cpu_pool, move |conn| {
            let store_subscription_repo = repo_factory.create_store_subscription_repo(&conn, user_id);

            let result = store_subscription_repo
                .get(StoreSubscriptionSearch::by_store_id(store_id))
                .map_err(ectx!(try convert))?;

            Ok(result.map(|result| StoreSubscriptionResponse {
                store_id: result.store_id,
                currency: result.currency.into(),
                value: result.value.to_super_unit(result.currency),
                wallet_address: result.wallet_address,
                trial_start_date: result.trial_start_date,
                trial_end_date: result.trial_start_date.map(|date| date + max_trial_duration),
                created_at: result.created_at,
                updated_at: result.updated_at,
                status: result.status,
            }))
        })
    }

    fn update(&self, store_id: StoreId, payload: UpdateStoreSubscriptionRequest) -> ServiceFutureV2<StoreSubscriptionResponse> {
        let repo_factory = self.repo_factory.clone();
        let user_id = self.dynamic_context.user_id;

        let db_pool = self.db_pool.clone();
        let cpu_pool = self.cpu_pool.clone();

        let max_trial_duration = Duration::days(self.config.trial_time_duration_days);

        let account_service = match self.dynamic_context.account_service.clone() {
            Some(account_service) => account_service,
            None => {
                let e = format_err!("Accounts service was not found in dynamic context");
                return Box::new(futures::future::err(ectx!(err e, ErrorKind::Internal))) as ServiceFutureV2<StoreSubscriptionResponse>;
            }
        };

        let fut = spawn_on_pool(db_pool, cpu_pool, move |conn| {
            let store_subscription_repo = repo_factory.create_store_subscription_repo(&conn, user_id);
            let by_store_id = StoreSubscriptionSearch::by_store_id(store_id);

            store_subscription_repo.get(by_store_id).map_err(ectx!(try convert))?.ok_or({
                let e = format_err!("Store subscription not found");
                ectx!(err e, ErrorKind::NotFound)
            })
        })
        .and_then(move |old_store_subscription| {
            let update_payload: UpdateStoreSubscription = payload.into();

            let new_currency = match update_payload.currency {
                Some(new_currency) if new_currency != old_store_subscription.currency => new_currency,
                _ => return Box::new(futures::future::ok(update_payload)) as ServiceFutureV2<UpdateStoreSubscription>,
            };

            match new_currency {
                Currency::Eur => Box::new(futures::future::ok(UpdateStoreSubscription {
                    currency: Some(Currency::Eur),
                    value: Some(Amount::new(DEFAULT_EUR_CENTS_AMOUNT)),
                    ..update_payload
                })) as ServiceFutureV2<UpdateStoreSubscription>,
                Currency::Stq => {
                    if old_store_subscription.wallet_address.is_none() {
                        let fut = account_service
                            .create_account(
                                Uuid::new_v4(),
                                format!("store_subscription_{}", old_store_subscription.store_id),
                                TureCurrency::Stq,
                                false,
                            )
                            .map(move |account| UpdateStoreSubscription {
                                currency: Some(Currency::Stq),
                                value: Some(Amount::new(DEFAULT_STQ_WEI_AMOUNT)),
                                wallet_address: Some(account.wallet_address),
                                ..update_payload
                            });
                        Box::new(fut) as ServiceFutureV2<UpdateStoreSubscription>
                    } else {
                        Box::new(futures::future::ok(UpdateStoreSubscription {
                            currency: Some(Currency::Stq),
                            value: Some(Amount::new(DEFAULT_STQ_WEI_AMOUNT)),
                            ..update_payload
                        })) as ServiceFutureV2<UpdateStoreSubscription>
                    }
                }
                Currency::Eth | Currency::Btc | Currency::Usd | Currency::Rub => {
                    let e = format_err!("Only {} and {} is allowed", Currency::Stq, Currency::Eur);
                    Box::new(futures::future::err(ectx!(err e, ErrorKind::Validation(serde_json::json!({
                        "currency": new_currency,
                    }))))) as ServiceFutureV2<UpdateStoreSubscription>
                }
            }
        })
        .and_then({
            let repo_factory = self.repo_factory.clone();
            let db_pool = self.db_pool.clone();
            let cpu_pool = self.cpu_pool.clone();
            move |store_subscription| {
                spawn_on_pool(db_pool, cpu_pool, move |conn| {
                    let store_subscription_repo = repo_factory.create_store_subscription_repo(&conn, user_id);
                    let by_store_id = StoreSubscriptionSearch::by_store_id(store_id);

                    let result = store_subscription_repo
                        .update(by_store_id, store_subscription)
                        .map_err(ectx!(try convert))?;
                    Ok(StoreSubscriptionResponse {
                        store_id: result.store_id,
                        currency: result.currency.into(),
                        value: result.value.to_super_unit(result.currency),
                        wallet_address: result.wallet_address,
                        trial_start_date: result.trial_start_date,
                        trial_end_date: result.trial_start_date.map(|date| date + max_trial_duration),
                        created_at: result.created_at,
                        updated_at: result.updated_at,
                        status: result.status,
                    })
                })
            }
        });

        Box::new(fut)
    }
}

fn create_store_subscription_account<AS: AccountService>(account_service: AS, store_id: StoreId) -> ServiceFutureV2<NewStoreSubscription> {
    let fut = account_service
        .create_account(Uuid::new_v4(), format!("store_subscription_{}", store_id), TureCurrency::Stq, false)
        .map(move |account| NewStoreSubscription {
            store_id,
            currency: Currency::Stq,
            value: Amount::new(DEFAULT_STQ_WEI_AMOUNT),
            wallet_address: Some(account.wallet_address),
            trial_start_date: None,
        });
    Box::new(fut)
}
