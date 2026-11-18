//! Fold the local receipt corpus into the venue catalog.
//!
//! The corpus is a partial slice. It does not replace a lens or a log walk.

use eyre::eyre;

use crate::catalog::Put;
use crate::{CorpusStore, DiscoveryStore};
use chain::decode::rows_from_log;
use journal::logs_in_block;
use store_model::decode_block_record;

pub async fn import_corpus<S>(store: &S) -> eyre::Result<u64>
where
    S: CorpusStore + DiscoveryStore + Clone + 'static,
{
    let cursor = store.cursor("corpus").await?.unwrap_or(0);
    let mut pending = Vec::new();
    let mut next = cursor;
    let mut wrote = 0_u64;
    let writer = store.clone();
    store
        .for_each_block(cursor, &mut |number, bytes| {
            next = number.saturating_add(1);
            if !bytes.is_empty() {
                let (record, _) = decode_block_record(bytes).map_err(|err| eyre!(err))?;
                for log in logs_in_block(&record)? {
                    pending.extend(rows_from_log(&log));
                }
            }
            if number % 2_000 == 0 {
                let batch = std::mem::take(&mut pending);
                let writer = writer.clone();
                let at = next;
                // The scan callback cannot await. A runtime `block_on` here
                // deadlocks the caller, so the commit runs on its own runtime.
                wrote += std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| eyre!("commit runtime: {err}"))?;
                    runtime.block_on(writer.commit_chunk("corpus", at, &batch, Put::Merge))
                })
                .join()
                .unwrap_or_else(|_| Err(eyre!("corpus commit panicked")))?;
                observability::info!(block = number, "corpus");
            }
            Ok(())
        })
        .await?;
    wrote += store
        .commit_chunk("corpus", next, &pending, Put::Merge)
        .await?;
    Ok(wrote)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use store_model::{Put, ReceiptBody, VenueRow};

    use super::import_corpus;
    use crate::{CorpusStore, DiscoveryStore};

    #[derive(Clone)]
    struct CursorStore {
        cursor: u64,
        start: Arc<Mutex<Option<u64>>>,
    }

    impl CorpusStore for CursorStore {
        async fn read_block(&self, _: u64) -> eyre::Result<Option<store_model::BlockRecord>> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_blocks(&self, _: &[store_model::BlockRecord]) -> eyre::Result<u64> {
            Err(eyre::eyre!("unused"))
        }
        async fn write_records(
            &self,
            _: &str,
            _: &[store_model::BlockRecord],
        ) -> eyre::Result<u64> {
            Err(eyre::eyre!("unused"))
        }
        async fn write_hole(&self, _: u64) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn meta(&self, _: &str) -> eyre::Result<Option<u64>> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_meta(&self, _: &str, _: u64) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn for_each_block(
            &self,
            from: u64,
            _: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
        ) -> eyre::Result<()> {
            *self.start.lock().expect("start") = Some(from);
            Ok(())
        }
    }

    impl DiscoveryStore for CursorStore {
        async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
            assert_eq!(job, "corpus");
            Ok(Some(self.cursor))
        }
        async fn commit_chunk(
            &self,
            job: &str,
            next: u64,
            _: &[VenueRow],
            _: Put,
        ) -> eyre::Result<u64> {
            assert_eq!(job, "corpus");
            assert_eq!(next, self.cursor);
            Ok(0)
        }
        async fn hit_scan(&self, _: &str) -> eyre::Result<Option<u64>> {
            Err(eyre::eyre!("unused"))
        }
        async fn hits_from(&self, _: &str, _: u64, _: u64) -> eyre::Result<Vec<u64>> {
            Err(eyre::eyre!("unused"))
        }
        async fn commit_hits(&self, _: &str, _: u64, _: &[u64]) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn read_bodies(&self, _: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_bodies(&self, _: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn note(&self, _: &str) -> eyre::Result<Option<u64>> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_note(&self, _: &str, _: u64) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
    }

    #[tokio::test]
    async fn the_corpus_fold_starts_at_the_stored_cursor() {
        let start = Arc::new(Mutex::new(None));
        let store = CursorStore {
            cursor: 26_440_000,
            start: Arc::clone(&start),
        };
        import_corpus(&store).await.unwrap();
        assert_eq!(*start.lock().expect("start"), Some(26_440_000));
    }
}
