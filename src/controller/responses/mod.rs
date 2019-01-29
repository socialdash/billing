use bigdecimal::ToPrimitive;
use chrono::NaiveDateTime;
use failure::Fail;
use stripe::{Card as StripeCard, CardBrand as StripeCardBrand};

use stq_types::{stripe::PaymentIntentId, UserId};

use models::{
    fee::FeeId,
    invoice_v2::InvoiceId,
    order_v2::{OrderId, RawOrder, StoreId},
    ChargeId, CustomerId, Fee, FeeStatus, PaymentIntent, PaymentIntentStatus, PaymentState,
};
use stq_static_resources::Currency as StqCurrency;

use services::error::{Error, ErrorContext, ErrorKind};

#[derive(Debug, Deserialize, Serialize)]
pub struct PaymentIntentResponse {
    pub id: PaymentIntentId,
    pub amount: f64,
    pub amount_received: f64,
    pub client_secret: Option<String>,
    pub currency: StqCurrency,
    pub last_payment_error_message: Option<String>,
    pub receipt_email: Option<String>,
    pub charge_id: Option<ChargeId>,
    pub status: PaymentIntentStatus,
}

impl PaymentIntentResponse {
    pub fn try_from_payment_intent(other: PaymentIntent) -> Result<Self, Error> {
        let other_amount = other.amount.to_super_unit(other.currency).to_f64();
        let other_amount_received = other.amount_received.to_super_unit(other.currency).to_f64();

        match (other_amount, other_amount_received) {
            (Some(amount), Some(amount_received)) => Ok(Self {
                id: other.id,
                amount,
                amount_received,
                client_secret: other.client_secret,
                currency: other.currency.into(),
                last_payment_error_message: other.last_payment_error_message,
                receipt_email: other.receipt_email,
                charge_id: other.charge_id,
                status: other.status,
            }),
            _ => Err(ectx!(err ErrorContext::AmountConversion, ErrorKind::Internal)),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderResponse {
    pub id: OrderId,
    pub seller_currency: StqCurrency,
    pub total_amount: f64,
    pub cashback_amount: f64,
    pub invoice_id: InvoiceId,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub store_id: StoreId,
    pub state: PaymentState,
}

impl OrderResponse {
    pub fn try_from_raw_order(raw_order: RawOrder) -> Result<Self, Error> {
        let total_amount = raw_order
            .total_amount
            .to_super_unit(raw_order.seller_currency)
            .to_f64()
            .ok_or(ectx!(try err ErrorContext::AmountConversion, ErrorKind::Internal))?;
        let cashback_amount = raw_order
            .cashback_amount
            .to_super_unit(raw_order.seller_currency)
            .to_f64()
            .ok_or(ectx!(try err ErrorContext::AmountConversion, ErrorKind::Internal))?;

        Ok(OrderResponse {
            id: raw_order.id,
            seller_currency: raw_order.seller_currency.into(),
            total_amount,
            cashback_amount,
            invoice_id: raw_order.invoice_id,
            created_at: raw_order.created_at,
            updated_at: raw_order.updated_at,
            store_id: raw_order.store_id,
            state: raw_order.state,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderSearchResultsResponse {
    pub total_count: i64,
    pub orders: Vec<OrderResponse>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CustomerResponse {
    pub id: CustomerId,
    pub user_id: UserId,
    pub email: Option<String>,
    pub cards: Vec<Card>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Card {
    pub id: String,
    pub brand: CardBrand,
    pub country: String,
    pub customer: Option<String>,
    pub exp_month: u32,
    pub exp_year: u32,
    pub last4: String,
    pub name: Option<String>,
}

impl From<StripeCard> for Card {
    fn from(other: StripeCard) -> Self {
        Self {
            id: other.id,
            brand: other.brand.into(),
            country: other.country,
            customer: other.customer,
            exp_month: other.exp_month,
            exp_year: other.exp_year,
            last4: other.last4,
            name: other.name,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone, Eq)]
pub enum CardBrand {
    AmericanExpress,
    DinersClub,
    Discover,
    JCB,
    Visa,
    MasterCard,
    UnionPay,
    #[serde(other)]
    Unknown,
}

impl From<StripeCardBrand> for CardBrand {
    fn from(other: StripeCardBrand) -> Self {
        match other {
            StripeCardBrand::AmericanExpress => CardBrand::AmericanExpress,
            StripeCardBrand::DinersClub => CardBrand::DinersClub,
            StripeCardBrand::Discover => CardBrand::Discover,
            StripeCardBrand::JCB => CardBrand::JCB,
            StripeCardBrand::Visa => CardBrand::Visa,
            StripeCardBrand::MasterCard => CardBrand::MasterCard,
            StripeCardBrand::UnionPay => CardBrand::UnionPay,
            StripeCardBrand::Unknown => CardBrand::Unknown,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeeResponse {
    pub id: FeeId,
    pub order_id: OrderId,
    pub amount: f64,
    pub status: FeeStatus,
    pub currency: StqCurrency,
    pub charge_id: Option<ChargeId>,
    pub metadata: Option<serde_json::Value>,
}

impl FeeResponse {
    pub fn try_from_fee(other: Fee) -> Result<Self, Error> {
        let other_amount = other.amount.to_super_unit(other.currency).to_f64();

        match other_amount {
            Some(amount) => Ok(Self {
                id: other.id,
                order_id: other.order_id,
                amount,
                status: other.status,
                currency: other.currency.into(),
                charge_id: other.charge_id,
                metadata: other.metadata,
            }),
            _ => Err(ectx!(err ErrorContext::AmountConversion, ErrorKind::Internal)),
        }
    }
}
