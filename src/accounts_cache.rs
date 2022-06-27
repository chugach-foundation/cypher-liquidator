use crate::liquidator::LiquidatorError;

use {
    dashmap::{mapref::one::Ref, DashMap},
    log::warn,
    solana_sdk::account::Account,
    solana_sdk::pubkey::Pubkey,
};

pub struct AccountsCache {
    map: DashMap<Pubkey, AccountState>,
}

#[derive(Debug)]
pub struct AccountState {
    pub account: Account,
    pub slot: u64,
}

impl AccountsCache {
    pub fn default() -> Self {
        Self {
            map: DashMap::default(),
        }
    }

    pub fn new() -> Self {
        AccountsCache {
            map: DashMap::new(),
        }
    }

    pub fn get(&self, key: &Pubkey) -> Option<Ref<'_, Pubkey, AccountState>> {
        self.map.get(key)
    }

    pub fn insert(&self, key: Pubkey, data: AccountState) -> Result<(), LiquidatorError> {
        self.map.insert(key, data);

        Ok(())
    }
}
