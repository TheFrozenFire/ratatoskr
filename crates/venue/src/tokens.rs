//! Token symbol and decimals.
//!
//! Discovery runs this after pool identities are written. The lens read lives
//! in [`chain::token`]. A token the batch cannot read is read on its own.

use alloy_primitives::Address;
use chain::rpc::Rpc;
use chain::seeds;
use chain::token::{self, ReadError, BATCH};
use eyre::eyre;

use crate::catalog::TokenRecord;
use crate::TokenStore;

const SCAN: usize = 65_536;

/// Wrapper links from the seed file. An empty symbol leaves a stored symbol.
pub(crate) async fn write_links<S: TokenStore>(store: &S) -> eyre::Result<()> {
    let mut rows = Vec::new();
    for wrap in &seeds::document().wraps {
        rows.push(TokenRecord {
            address: wrap.token,
            symbol: String::new(),
            decimals: None,
            wraps: Some(wrap.wrapper),
        });
        if wrap.wrapper != Address::ZERO {
            rows.push(TokenRecord {
                address: wrap.wrapper,
                symbol: String::new(),
                decimals: None,
                wraps: Some(wrap.token),
            });
        }
    }
    store.put_tokens(&rows).await
}

/// Read symbol and decimals for the wrapper and for tokens that have no record.
pub(crate) async fn walk<S: TokenStore>(
    rpc: &Rpc,
    store: &S,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<u64> {
    observability::note_token_reading(true);
    let result = read_missing(rpc, store, stop).await;
    observability::note_token_reading(false);
    result
}

async fn read_missing<S: TokenStore>(
    rpc: &Rpc,
    store: &S,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<u64> {
    write_links(store).await?;
    let mut wrote = fill(rpc, store, &blank_wrappers(store).await?, stop).await?;
    loop {
        if *stop.borrow() {
            return Ok(wrote);
        }
        let missing = store.missing_tokens(SCAN).await?;
        observability::note_token_lens(missing.len() as u64, missing.len() == SCAN);
        if missing.is_empty() {
            break;
        }
        wrote += fill(rpc, store, &missing, stop).await?;
    }
    Ok(wrote)
}

async fn blank_wrappers<S: TokenStore>(store: &S) -> eyre::Result<Vec<Address>> {
    let mut out = Vec::new();
    for wrap in &seeds::document().wraps {
        if wrap.wrapper == Address::ZERO || out.contains(&wrap.wrapper) {
            continue;
        }
        match store.token(wrap.wrapper).await? {
            Some(row) if !row.symbol.is_empty() || row.decimals.is_some() => {}
            _ => out.push(wrap.wrapper),
        }
    }
    Ok(out)
}

async fn fill<S: TokenStore>(
    rpc: &Rpc,
    store: &S,
    tokens: &[Address],
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<u64> {
    let mut wrote = 0_u64;
    let mut at = 0;
    while at < tokens.len() {
        if crate::hold_for_pause(stop).await {
            return Ok(wrote);
        }
        let mut count = (tokens.len() - at).min(BATCH).max(1);
        loop {
            let batch = tokens[at..at + count].to_vec();
            let rpc_fetch = rpc.clone();
            let fetched =
                match tokio::task::spawn_blocking(move || token::read_batch(&rpc_fetch, &batch))
                    .await
                {
                    Ok(fetched) => fetched,
                    Err(err) => Err(ReadError::Fatal(eyre!("token lens: {err}"))),
                };
            match fetched {
                Ok(rows) => {
                    let n = rows.len() as u64;
                    store.put_tokens(&rows).await?;
                    wrote += n;
                    observability::info!(at, count, rows = n, "token lens");
                    at += count;
                    break;
                }
                Err(ReadError::Shrink(reason)) if count > 1 => {
                    let smaller = (count / 2).max(1);
                    observability::warn!(at, count, smaller, %reason, "token lens shrink");
                    count = smaller;
                }
                Err(ReadError::Shrink(reason)) => {
                    let token = tokens[at];
                    observability::warn!(%token, %reason, "token lens direct");
                    let rpc_one = rpc.clone();
                    let row = tokio::task::spawn_blocking(move || token::read_one(&rpc_one, token))
                        .await
                        .map_err(|err| eyre!("token {token}: {err}"))??;
                    store.put_tokens(&[row]).await?;
                    wrote += 1;
                    at += 1;
                    break;
                }
                Err(ReadError::Fatal(err)) => return Err(err),
            }
        }
    }
    Ok(wrote)
}
