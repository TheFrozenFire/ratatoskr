//! Discovery cursors, bloom hits, era1 bodies, and seed notes.

use api::{Op, Reply, READ_CAP};
use eyre::eyre;
use store_model::{Put, ReceiptBody, VenueRow};

use crate::remote::{empty, wrote, RemoteStore};
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

impl DiscoveryStore for RemoteStore {
    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        match self
            .request(Op::Cursor {
                job: job.to_string(),
            })
            .await?
        {
            Reply::Cursor(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64> {
        wrote(
            self.request(Op::CommitChunk {
                job: job.to_string(),
                next,
                rows: rows.to_vec(),
                put,
            })
            .await?,
        )
    }

    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        match self
            .request(Op::HitScan {
                job: job.to_string(),
            })
            .await?
        {
            Reply::HitScan(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>> {
        if from >= to {
            return Ok(Vec::new());
        }
        let mut from = from;
        let mut hits = Vec::new();
        loop {
            let page = match self
                .request(Op::HitsFrom {
                    job: job.to_string(),
                    from,
                    to,
                })
                .await?
            {
                Reply::Hits(page) => page,
                _ => return Err(eyre!("unexpected reply")),
            };
            let short = (page.len() as u64) < READ_CAP;
            let Some(last) = page.last().copied() else {
                break;
            };
            let next = last.saturating_add(1);
            hits.extend(page);
            if short || next <= from || next >= to {
                break;
            }
            from = next;
        }
        Ok(hits)
    }

    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()> {
        empty(
            self.request(Op::CommitHits {
                job: job.to_string(),
                scanned_to,
                hits: hits.to_vec(),
            })
            .await?,
        )
    }

    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
        let mut out = Vec::with_capacity(numbers.len());
        for chunk in numbers.chunks(READ_CAP as usize) {
            let part = match self
                .request(Op::ReadBodies {
                    numbers: chunk.to_vec(),
                })
                .await?
            {
                Reply::Bodies(part) => part,
                _ => return Err(eyre!("unexpected reply")),
            };
            if part.len() != chunk.len() {
                return Err(eyre!("bodies"));
            }
            out.extend(part);
        }
        Ok(out)
    }

    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
        empty(
            self.request(Op::PutBodies {
                bodies: bodies.to_vec(),
            })
            .await?,
        )
    }

    async fn note(&self, _key: &str) -> eyre::Result<Option<u64>> {
        Err(eyre!("discovery notes are not open"))
    }

    async fn put_note(&self, _key: &str, _value: u64) -> eyre::Result<()> {
        Err(eyre!("discovery notes are not open"))
    }
}
