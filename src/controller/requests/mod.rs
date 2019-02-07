use models::order_v2::OrderId as Orderv2Id;
use models::{CustomerId, PaymentState};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewCustomerWithSourceRequest {
    pub email: Option<String>,
    pub card_token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeleteCustomerRequest {
    pub customer_id: CustomerId,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateCustomerRequest {
    pub email: Option<String>,
    pub card_token: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct OrderPaymentStateRequest {
    pub state: PaymentState,
}

#[derive(Deserialize, Debug, Clone)]
pub struct FeesPayByOrdersRequest {
    pub order_ids: Vec<Orderv2Id>,
}
