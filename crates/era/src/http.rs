//! One pooled HTTP client for range reads.
//!
//! The mirror negotiates HTTP/2. Many small ranges then share a connection
//! and run up to `inflight` streams at once, instead of one round trip per
//! block on a fresh TCP socket. Idle HTTP/1.1 connections are kept up to the
//! same limit for a mirror that does not offer HTTP/2. Nagle is off because
//! the reads are a few hundred bytes. The HTTP/2 window grows with the
//! receipt bodies.

use eyre::{eyre, WrapErr};
use reqwest::header::RANGE;
use std::{
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::sleep;

#[derive(Clone, Copy, Debug)]
pub enum Bucket {
    Catalog,
    Index,
    Header,
    Receipt,
}

#[derive(Default)]
struct Stats {
    catalog: AtomicU64,
    index: AtomicU64,
    header: AtomicU64,
    receipt: AtomicU64,
    requests: AtomicU64,
    /// 0 unknown. Otherwise the oldest version seen, so a fallback to
    /// HTTP/1.1 is visible: 1 HTTP/1.0, 2 HTTP/1.1, 3 HTTP/2, 4 HTTP/3.
    version: AtomicU8,
}

#[derive(Clone, Default)]
pub struct Transfer {
    pub catalog: u64,
    pub index: u64,
    pub header: u64,
    pub receipt: u64,
    pub requests: u64,
    pub version: &'static str,
}

impl Transfer {
    pub fn total(&self) -> u64 {
        self.catalog + self.index + self.header + self.receipt
    }
}

#[derive(Clone)]
pub struct EraHttp {
    client: reqwest::Client,
    stats: Arc<Stats>,
}

impl EraHttp {
    pub fn new(inflight: usize) -> eyre::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("ratatoskr-era/0.1")
            .pool_max_idle_per_host(inflight.max(1))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(30))
            .https_only(true)
            // Default HTTP/2 windows are 64 KiB. A header wave is hundreds of
            // those, and one receipt can be larger than the stream window, so
            // start big enough that the first flight is not stalled on
            // WINDOW_UPDATE. Adaptive growth still applies above this.
            .http2_adaptive_window(true)
            .http2_initial_stream_window_size(1024 * 1024)
            .http2_initial_connection_window_size(8 * 1024 * 1024)
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .wrap_err("build http client")?;
        Ok(Self {
            client,
            stats: Arc::new(Stats::default()),
        })
    }

    pub fn transfer(&self) -> Transfer {
        let version = match self.stats.version.load(Ordering::Relaxed) {
            1 => "HTTP/1.0",
            2 => "HTTP/1.1",
            3 => "HTTP/2",
            4 => "HTTP/3",
            _ => "unknown",
        };
        Transfer {
            catalog: self.stats.catalog.load(Ordering::Relaxed),
            index: self.stats.index.load(Ordering::Relaxed),
            header: self.stats.header.load(Ordering::Relaxed),
            receipt: self.stats.receipt.load(Ordering::Relaxed),
            requests: self.stats.requests.load(Ordering::Relaxed),
            version,
        }
    }

    pub async fn get_text(&self, url: &str) -> eyre::Result<String> {
        let bytes = self.request(url, None, Bucket::Catalog).await?;
        String::from_utf8(bytes.to_vec()).wrap_err("listing is not utf-8")
    }

    /// Object length from a one-byte range, used when a listing omits sizes.
    pub async fn object_len(&self, url: &str) -> eyre::Result<u64> {
        let response = self
            .client
            .get(url)
            .header(RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|err| eyre!(err))
            .wrap_err("probe era1 length")?;
        self.note_version(response.version());
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(eyre!("length probe returned HTTP {}", response.status()));
        }
        let total = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit('/').next())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|total| *total > 0)
            .ok_or_else(|| eyre!("mirror did not return the era1 object length"))?;
        let bytes = response.bytes().await.wrap_err("length probe body")?;
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        self.stats
            .catalog
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(total)
    }

    /// Inclusive range. Refuses a 200, which would be the whole object.
    pub async fn get_exact(
        &self,
        url: &str,
        start: u64,
        end: u64,
        bucket: Bucket,
    ) -> eyre::Result<Vec<u8>> {
        if end < start {
            return Err(eyre!("empty range {start}-{end}"));
        }
        let bytes = self.request(url, Some((start, end)), bucket).await?;
        let want = end - start + 1;
        if bytes.len() as u64 != want {
            return Err(eyre!(
                "range {start}-{end} returned {} bytes, wanted {want}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    async fn request(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        bucket: Bucket,
    ) -> eyre::Result<Vec<u8>> {
        let mut last = None;
        for attempt in 1..=4 {
            match self.once(url, range, bucket).await {
                Ok(bytes) => return Ok(bytes),
                Err(err) if attempt < 4 && err.retry => {
                    tracing::warn!(attempt, error = %err.report, "retrying http read");
                    sleep(Duration::from_millis(40 * attempt)).await;
                    last = Some(err.report);
                }
                Err(err) => return Err(err.report),
            }
        }
        Err(last.unwrap_or_else(|| eyre!("http read failed")))
    }

    async fn once(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        bucket: Bucket,
    ) -> Result<Vec<u8>, Attempt> {
        let mut builder = self.client.get(url);
        if let Some((start, end)) = range {
            builder = builder.header(RANGE, format!("bytes={start}-{end}"));
        }
        let response = builder.send().await.map_err(retriable_reqwest)?;
        self.note_version(response.version());
        let status = response.status();
        if range.is_some() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            // Drop the body without reading it. A 200 here is the whole file.
            return Err(fatal(eyre!("range request returned HTTP {status}")));
        }
        if range.is_none() && !status.is_success() {
            let retry = status.as_u16() == 429 || status.is_server_error();
            return Err(Attempt {
                retry,
                report: eyre!("GET returned HTTP {status}"),
            });
        }
        if let Some(length) = response.content_length() {
            if let Some((start, end)) = range {
                let want = end - start + 1;
                if length != want {
                    return Err(fatal(eyre!(
                        "range {start}-{end} content-length {length}, wanted {want}"
                    )));
                }
            }
        }
        let bytes = response.bytes().await.map_err(retriable_reqwest)?;
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let counter = match bucket {
            Bucket::Catalog => &self.stats.catalog,
            Bucket::Index => &self.stats.index,
            Bucket::Header => &self.stats.header,
            Bucket::Receipt => &self.stats.receipt,
        };
        counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes.to_vec())
    }

    fn note_version(&self, version: reqwest::Version) {
        let code = match version {
            reqwest::Version::HTTP_09 | reqwest::Version::HTTP_10 => 1,
            reqwest::Version::HTTP_11 => 2,
            reqwest::Version::HTTP_2 => 3,
            reqwest::Version::HTTP_3 => 4,
            _ => return,
        };
        let mut current = self.stats.version.load(Ordering::Relaxed);
        loop {
            let next = if current == 0 {
                code
            } else {
                current.min(code)
            };
            match self.stats.version.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
    }
}

struct Attempt {
    retry: bool,
    report: eyre::Report,
}

fn fatal(report: eyre::Report) -> Attempt {
    Attempt {
        retry: false,
        report,
    }
}

fn retriable_reqwest(err: reqwest::Error) -> Attempt {
    let retry = err.is_timeout() || err.is_connect() || err.is_request();
    Attempt {
        retry,
        report: eyre::Report::from(err),
    }
}
