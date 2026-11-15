//! Token records.

use alloy_primitives::Address;
use store_model::TokenRecord;

use crate::{blocking, LocalStore};

#[allow(async_fn_in_trait)]
pub trait TokenStore: Send + Sync {
    async fn put_tokens(&self, rows: &[TokenRecord]) -> eyre::Result<()>;
    async fn tokens(&self) -> eyre::Result<Vec<TokenRecord>>;
    async fn token(&self, address: Address) -> eyre::Result<Option<TokenRecord>>;
    async fn missing_tokens(&self, limit: usize) -> eyre::Result<Vec<Address>>;
}

impl TokenStore for LocalStore {
    async fn put_tokens(&self, rows: &[TokenRecord]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let rows = rows.to_vec();
        blocking(move || catalog.put_tokens(&rows)).await
    }

    async fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.tokens()).await
    }

    async fn token(&self, address: Address) -> eyre::Result<Option<TokenRecord>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.token(address)).await
    }

    async fn missing_tokens(&self, limit: usize) -> eyre::Result<Vec<Address>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.missing_tokens(limit)).await
    }
}
