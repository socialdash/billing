use std::str::FromStr;
use std::time::SystemTime;

use stq_static_resources::*;
use stq_types::*;

use models::Order;

table! {
    invoices (id) {
        id -> Uuid,
        invoice_id -> Uuid,
        transaction_id -> Nullable<VarChar>,
        transaction_captured_amount -> Double,
        amount -> Double,
        currency_id -> Integer,
        price_reserved -> Timestamp, // UTC 0, generated at db level
        state -> VarChar,
        wallet -> Nullable<VarChar>,
    }
}

#[derive(Serialize, Deserialize, Queryable, Insertable, AsChangeset, Debug, Clone)]
#[table_name = "invoices"]
pub struct Invoice {
    pub id: SagaId,
    pub invoice_id: InvoiceId,
    pub transaction_id: Option<String>,
    pub transaction_captured_amount: ProductPrice,
    pub amount: ProductPrice,
    pub currency_id: CurrencyId,
    pub price_reserved: SystemTime,
    pub state: OrderState,
    pub wallet: Option<String>,
}

impl Invoice {
    pub fn new(id: SagaId, external_invoice: ExternalBillingInvoice) -> Self {
        let currency_id = CurrencyId(Currency::from_str(&external_invoice.currency).unwrap_or_else(|_| Currency::Stq) as i32);
        let state = match external_invoice.status {
            ExternalBillingStatus::New | ExternalBillingStatus::Wallet => OrderState::PaymentAwaited,
            ExternalBillingStatus::Waiting => OrderState::TransactionPending,
            ExternalBillingStatus::Done => OrderState::Paid,
        };
        let transaction_captured_amount = ProductPrice(f64::from_str(&external_invoice.amount_captured).unwrap_or_default());
        let amount = ProductPrice(f64::from_str(&external_invoice.amount).unwrap_or_default());
        Self {
            id,
            invoice_id: external_invoice.id,
            transaction_id: external_invoice.transaction_id,
            transaction_captured_amount,
            amount,
            currency_id,
            price_reserved: SystemTime::now(), //TODO: ON EXTERNAL BILLING SIDE
            state,
            wallet: external_invoice.wallet,
        }
    }
}

#[derive(Serialize, Deserialize, Queryable, Insertable, AsChangeset, Debug, Clone)]
#[table_name = "invoices"]
pub struct UpdateInvoice {
    pub invoice_id: InvoiceId,
    pub transaction_id: Option<String>,
    pub transaction_captured_amount: ProductPrice,
    pub amount: ProductPrice,
    pub currency_id: CurrencyId,
    pub price_reserved: SystemTime,
    pub state: OrderState,
    pub wallet: Option<String>,
}

impl From<ExternalBillingInvoice> for UpdateInvoice {
    fn from(external_invoice: ExternalBillingInvoice) -> Self {
        let currency_id = CurrencyId(Currency::from_str(&external_invoice.currency).unwrap_or_else(|_| Currency::Stq) as i32);
        let state = match external_invoice.status {
            ExternalBillingStatus::New | ExternalBillingStatus::Wallet => OrderState::PaymentAwaited,
            ExternalBillingStatus::Waiting => OrderState::TransactionPending,
            ExternalBillingStatus::Done => OrderState::Paid,
        };
        let transaction_captured_amount = ProductPrice(f64::from_str(&external_invoice.amount_captured).unwrap_or_default());
        let amount = ProductPrice(f64::from_str(&external_invoice.amount).unwrap_or_default());
        Self {
            invoice_id: external_invoice.id,
            transaction_id: external_invoice.transaction_id,
            transaction_captured_amount,
            amount,
            currency_id,
            price_reserved: SystemTime::now(), //TODO: ON EXTERNAL BILLING SIDE
            state,
            wallet: external_invoice.wallet,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BillingOrder {
    pub merchant: MerchantId,
    pub amount: ProductPrice,
    pub currency: String,
    pub description: Option<String>,
}

impl BillingOrder {
    pub fn new(order: Order, merchant: MerchantId) -> Self {
        Self {
            merchant,
            amount: order.price,
            currency: order.currency_id.to_string(),
            description: Some(format!("Order - id : {}", order.id)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateInvoicePayload {
    callback_url: String,
    currency: String,
    amount: ProductPrice,
    timeout_s: i32,
    purchases: Vec<BillingOrder>,
}

impl CreateInvoicePayload {
    pub fn new(purchases: Vec<BillingOrder>, callback_url: String, currency: String, timeout_s: i32) -> Self {
        let amount = purchases.iter().fold(0.0, |acc, x| acc + x.amount.0); //TODO: ON EXTERNAL BILLING SIDE
        let amount = ProductPrice(amount);
        Self {
            purchases,
            callback_url,
            currency,
            amount,
            timeout_s,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExternalBillingInvoice {
    pub id: InvoiceId,
    pub amount_captured: String,
    pub transaction_id: Option<String>,
    pub wallet: Option<String>,
    pub amount: String,
    pub currency: String,
    pub status: ExternalBillingStatus,
    //pub price_reserved: SystemTime,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ExternalBillingStatus {
    New,
    Wallet,
    Waiting,
    Done,
}