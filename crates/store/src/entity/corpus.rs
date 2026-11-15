//! Receipt corpus.

use store_model::BlockRecord;

use crate::{blocking, LocalStore};

#[allow(async_fn_in_trait)]
pub trait CorpusStore: Send + Sync {
    async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>>;
    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64>;
    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64>;
    async fn write_hole(&self, number: u64) -> eyre::Result<()>;
    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>>;
    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()>;
    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()>;
}

impl CorpusStore for LocalStore {
    async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>> {
        let corpus = self.corpus_db()?;
        blocking(move || corpus.read_block(number)).await
    }

    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64> {
        let corpus = self.corpus_db()?;
        let records = records.to_vec();
        blocking(move || corpus.put_blocks(&records)).await
    }

    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64> {
        let corpus = self.corpus_db()?;
        let cursor_key = cursor_key.to_string();
        let records = records.to_vec();
        blocking(move || corpus.write_records(&cursor_key, &records)).await
    }

    async fn write_hole(&self, number: u64) -> eyre::Result<()> {
        let corpus = self.corpus_db()?;
        blocking(move || corpus.write_hole(number)).await
    }

    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>> {
        let corpus = self.corpus_db()?;
        let key = key.to_string();
        blocking(move || corpus.meta(&key)).await
    }

    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()> {
        let corpus = self.corpus_db()?;
        let key = key.to_string();
        blocking(move || corpus.put_meta(&key, value)).await
    }

    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        let mut from = 0_u64;
        loop {
            let corpus = self.corpus_db()?;
            let start = from;
            let page_bytes = self.caches.block_page;
            let page = blocking(move || corpus.read_block_page(start, page_bytes)).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            let next = last.saturating_add(1);
            for (number, bytes) in &page {
                visit(*number, bytes)?;
            }
            if next <= from {
                break;
            }
            from = next;
        }
        Ok(())
    }
}
