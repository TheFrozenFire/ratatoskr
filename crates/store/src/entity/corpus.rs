//! Receipt corpus.

use api::{Op, Reply, SCAN_PAGE};
use eyre::eyre;
use model::BlockRecord;

use crate::remote::{empty, wrote, RemoteStore};
use crate::{blocking, LocalStore};

/// Block bytes copied out of one corpus read. The table is multiple gigabytes,
/// so the next page is read only after this one has been visited.
const BLOCK_PAGE: usize = 8 * 1024 * 1024;

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
            let page = blocking(move || corpus.read_block_page(start, BLOCK_PAGE)).await?;
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

impl CorpusStore for RemoteStore {
    async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>> {
        match self.request(Op::ReadBlock { number }).await? {
            Reply::Block(record) => Ok(record),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64> {
        wrote(
            self.request(Op::PutBlocks {
                records: records.to_vec(),
            })
            .await?,
        )
    }

    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64> {
        wrote(
            self.request(Op::WriteRecords {
                cursor_key: cursor_key.to_string(),
                records: records.to_vec(),
            })
            .await?,
        )
    }

    async fn write_hole(&self, number: u64) -> eyre::Result<()> {
        empty(self.request(Op::WriteHole { number }).await?)
    }

    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>> {
        match self
            .request(Op::Meta {
                key: key.to_string(),
            })
            .await?
        {
            Reply::Meta(value) => Ok(value),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()> {
        empty(
            self.request(Op::PutMeta {
                key: key.to_string(),
                value,
            })
            .await?,
        )
    }

    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        let mut after = None;
        loop {
            let page = match self
                .request(Op::ScanBlocks {
                    after,
                    limit: SCAN_PAGE,
                })
                .await?
            {
                Reply::Scan(page) => page,
                _ => return Err(eyre!("unexpected reply")),
            };
            let short = (page.len() as u64) < SCAN_PAGE;
            let end = page.last().map(|(number, _)| *number);
            for (number, bytes) in page {
                visit(number, &bytes)?;
            }
            if short {
                break;
            }
            let Some(end) = end else {
                break;
            };
            if after.is_some_and(|after| end <= after) {
                break;
            }
            after = Some(end);
        }
        Ok(())
    }
}
