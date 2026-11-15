//! Discovery cursors, bloom hits, era1 bodies, and seed notes.

use store_model::{Put, ReceiptBody, VenueRow};

use crate::{blocking, LocalStore};

#[allow(async_fn_in_trait)]
pub trait DiscoveryStore: Send + Sync {
    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>>;
    async fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64>;
    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>>;
    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>>;
    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()>;
    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>>;
    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()>;
    async fn note(&self, key: &str) -> eyre::Result<Option<u64>>;
    async fn put_note(&self, key: &str, value: u64) -> eyre::Result<()>;
}

impl DiscoveryStore for LocalStore {
    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        blocking(move || catalog.cursor(&job)).await
    }

    async fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        let rows = rows.to_vec();
        blocking(move || catalog.commit_chunk(&job, next, &rows, put)).await
    }

    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        blocking(move || catalog.hit_scan(&job)).await
    }

    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        blocking(move || catalog.hits_from(&job, from, to)).await
    }

    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        let hits = hits.to_vec();
        blocking(move || catalog.commit_hits(&job, scanned_to, &hits)).await
    }

    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
        let catalog = self.catalog_db()?;
        let numbers = numbers.to_vec();
        blocking(move || catalog.read_bodies(&numbers)).await
    }

    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let bodies = bodies.to_vec();
        blocking(move || catalog.put_bodies(&bodies)).await
    }

    async fn note(&self, key: &str) -> eyre::Result<Option<u64>> {
        let catalog = self.catalog_db()?;
        let key = key.to_string();
        blocking(move || catalog.note(&key)).await
    }

    async fn put_note(&self, key: &str, value: u64) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let key = key.to_string();
        blocking(move || catalog.put_note(&key, value)).await
    }
}
