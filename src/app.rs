use crate::account::AccountStore;
use std::path::PathBuf;

pub struct App {
    pub accounts: AccountStore,
    pub store_path: PathBuf,
}

impl App {
    pub fn new(accounts: AccountStore, store_path: PathBuf) -> Self {
        App {
            accounts,
            store_path,
        }
    }
}
