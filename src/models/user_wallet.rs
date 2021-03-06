use chrono::NaiveDateTime;
use std::fmt;
use uuid::Uuid;

use models::{TureCurrency, UserId};
use schema::user_wallets;

#[derive(Clone, Copy, Debug, PartialEq, Eq, From, FromStr, Hash, Serialize, Deserialize, DieselTypes)]
pub struct UserWalletId(Uuid);

impl UserWalletId {
    pub fn new(id: Uuid) -> Self {
        UserWalletId(id)
    }

    pub fn inner(&self) -> &Uuid {
        &self.0
    }

    pub fn into_inner(self) -> Uuid {
        self.0
    }

    pub fn generate() -> Self {
        UserWalletId(Uuid::new_v4())
    }
}

impl fmt::Display for UserWalletId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&format!("{}", self.0.hyphenated()))
    }
}

#[derive(Clone, Debug, Display, PartialEq, Eq, From, FromStr, Hash, Serialize, Deserialize, DieselTypes)]
pub struct WalletAddress(String);

impl WalletAddress {
    pub fn new(address: String) -> Self {
        WalletAddress(address)
    }

    pub fn inner(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct UserWallet {
    pub id: UserWalletId,
    pub address: WalletAddress,
    pub currency: TureCurrency,
    pub user_id: UserId,
    pub created_at: NaiveDateTime,
    pub is_active: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NewActiveUserWallet {
    pub id: UserWalletId,
    pub address: WalletAddress,
    pub currency: TureCurrency,
    pub user_id: UserId,
}

#[derive(Clone, Debug, Deserialize, Queryable)]
pub struct RawUserWallet {
    pub id: UserWalletId,
    pub address: WalletAddress,
    pub currency: TureCurrency,
    pub user_id: UserId,
    pub created_at: NaiveDateTime,
    pub is_active: bool,
}

impl From<RawUserWallet> for UserWallet {
    fn from(raw_user_wallet: RawUserWallet) -> Self {
        let RawUserWallet {
            id,
            address,
            currency,
            user_id,
            created_at,
            is_active,
        } = raw_user_wallet;

        Self {
            id,
            address,
            currency,
            user_id,
            created_at,
            is_active,
        }
    }
}

#[derive(Clone, Debug, Serialize, Insertable)]
#[table_name = "user_wallets"]
pub struct InsertUserWallet {
    pub id: UserWalletId,
    pub address: WalletAddress,
    pub currency: TureCurrency,
    pub user_id: UserId,
    pub is_active: bool,
}

impl From<NewActiveUserWallet> for InsertUserWallet {
    fn from(new_wallet: NewActiveUserWallet) -> Self {
        let NewActiveUserWallet {
            id,
            address,
            currency,
            user_id,
        } = new_wallet;

        Self {
            id,
            address,
            currency,
            user_id,
            is_active: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UserWalletAccess {
    pub user_id: UserId,
}

impl From<&UserWallet> for UserWalletAccess {
    fn from(user_wallet: &UserWallet) -> UserWalletAccess {
        UserWalletAccess {
            user_id: user_wallet.user_id.clone(),
        }
    }
}

impl From<&NewActiveUserWallet> for UserWalletAccess {
    fn from(new_wallet: &NewActiveUserWallet) -> UserWalletAccess {
        UserWalletAccess {
            user_id: new_wallet.user_id.clone(),
        }
    }
}
