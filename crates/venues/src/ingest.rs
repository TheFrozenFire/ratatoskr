//! Read the seed file into catalog notes and token records.
//!
//! A list root is probed for the view that answers. A log root's first block
//! with code is found by binary search. The Curve address provider is asked
//! for registries. Symbols and decimals are calls on the token.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{keccak256, Address};
use eyre::eyre;

use crate::catalog::{Put, TokenRecord, CURVE};
use crate::lists::{self, ListJob};
use crate::logs::{self, LogJob};
use crate::rpc::Rpc;
use crate::seeds::{self, Document};
use crate::EntityStore;

const TOKEN_BATCH: usize = 256;
const CALL_GAS: u64 = 1_000_000;
const NO_VIEW: u64 = u64::MAX;
const SEL_DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
const SEL_SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];

pub(crate) struct Plan {
    pub lists: Vec<ListJob>,
    pub logs: Vec<LogJob>,
}

struct RegistryFound {
    id: u64,
    address: Address,
    shape: Option<u64>,
}

pub(crate) async fn plan<S: EntityStore>(store: &S, rpc: &Rpc) -> eyre::Result<Plan> {
    write_wraps(store, rpc).await?;
    write_token_batch(store, rpc).await?;
    let doc = seeds::document();
    let mut shapes = HashMap::new();
    let mut deploys = HashMap::new();
    for root in &doc.roots {
        if root.registries {
            continue;
        }
        if root.list {
            shapes.insert(
                root.name.clone(),
                shape_of(store, rpc, &root.name, root.address).await?,
            );
        }
        if root.logs() {
            if let Some(block) = deploy_of(store, rpc, &root.name, root.address).await? {
                deploys.insert(root.name.clone(), block);
            }
        }
    }
    let registries = match doc.roots.iter().find(|root| root.registries) {
        Some(provider) => read_provider(store, rpc, provider.address).await?,
        None => Vec::new(),
    };
    Ok(assemble(doc, &shapes, &deploys, &registries))
}

/// Log jobs from stored deploy notes. A missing note starts at genesis.
pub(crate) async fn cached_logs<S: EntityStore>(store: &S) -> Vec<LogJob> {
    let doc = seeds::document();
    let mut jobs = Vec::new();
    for root in &doc.roots {
        if !root.logs() {
            continue;
        }
        let deploy = store
            .note(&deploy_key(&root.name))
            .await
            .ok()
            .flatten()
            .unwrap_or(0);
        jobs.push(logs::log_job(
            &log_name(&root.name),
            root.address,
            root.topics.clone(),
            deploy,
        ));
    }
    jobs
}

fn assemble(
    doc: &Document,
    shapes: &HashMap<String, u64>,
    deploys: &HashMap<String, u64>,
    registries: &[RegistryFound],
) -> Plan {
    let mut lists = Vec::new();
    let mut logs = Vec::new();
    for root in &doc.roots {
        if root.registries {
            continue;
        }
        if let Some(view) = shapes.get(&root.name).copied().and_then(view_of) {
            lists.push(lists::job_from(
                &list_name(&root.name),
                root.address,
                root.kind,
                view,
                Put::Merge,
            ));
        }
        if root.logs() {
            logs.push(logs::log_job(
                &log_name(&root.name),
                root.address,
                root.topics.clone(),
                deploys.get(&root.name).copied().unwrap_or(0),
            ));
        }
    }
    for found in registries {
        if doc.by_address(found.address).is_some() {
            continue;
        }
        let Some(view) = found.shape.and_then(view_of) else {
            continue;
        };
        lists.push(lists::job_from(
            &list_name(&format!("curve-id-{}", found.id)),
            found.address,
            CURVE,
            view,
            Put::IfAbsent,
        ));
    }
    Plan { lists, logs }
}

async fn write_wraps<S: EntityStore>(store: &S, rpc: &Rpc) -> eyre::Result<()> {
    let mut rows = Vec::new();
    for wrap in &seeds::document().wraps {
        rows.push(TokenRecord {
            address: wrap.token,
            symbol: String::new(),
            decimals: None,
            wraps: Some(wrap.wrapper),
        });
        let (symbol, decimals) = if wrap.wrapper == Address::ZERO {
            (String::new(), None)
        } else {
            let rpc = rpc.clone();
            let wrapper = wrap.wrapper;
            block_on(move || read_erc20(&rpc, wrapper))
                .await
                .unwrap_or((String::new(), None))
        };
        rows.push(TokenRecord {
            address: wrap.wrapper,
            symbol,
            decimals,
            wraps: Some(wrap.token),
        });
    }
    store.put_tokens(&rows).await
}

async fn write_token_batch<S: EntityStore>(store: &S, rpc: &Rpc) -> eyre::Result<()> {
    let missing = store.missing_tokens(TOKEN_BATCH).await?;
    let mut rows = Vec::new();
    for token in missing {
        if token == Address::ZERO {
            continue;
        }
        let rpc = rpc.clone();
        let (symbol, decimals) = block_on(move || read_erc20(&rpc, token)).await?;
        rows.push(TokenRecord {
            address: token,
            symbol,
            decimals,
            wraps: None,
        });
    }
    store.put_tokens(&rows).await
}

async fn shape_of<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    name: &str,
    factory: Address,
) -> eyre::Result<u64> {
    let key = shape_key(name);
    if let Some(stored) = store.note(&key).await? {
        return Ok(stored);
    }
    let rpc = rpc.clone();
    let view = block_on(move || probe_view(&rpc, factory)).await?;
    let stored = match view {
        Some(index) => index as u64 + 1,
        None => NO_VIEW,
    };
    store.put_note(&key, stored).await?;
    Ok(stored)
}

async fn deploy_of<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    name: &str,
    address: Address,
) -> eyre::Result<Option<u64>> {
    let key = deploy_key(name);
    if let Some(stored) = store.note(&key).await? {
        return Ok(Some(stored));
    }
    let rpc = rpc.clone();
    let found = block_on(move || search_deploy(&rpc, address)).await?;
    if let Some(block) = found {
        store.put_note(&key, block).await?;
    }
    Ok(found)
}

async fn read_provider<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    provider: Address,
) -> eyre::Result<Vec<RegistryFound>> {
    let rpc_read = rpc.clone();
    let found = block_on(move || read_registries(&rpc_read, provider)).await?;
    let mut rows = Vec::new();
    for (id, address) in found {
        if seeds::document().by_address(address).is_some() {
            continue;
        }
        let name = format!("curve-id-{id}");
        let shape = shape_of(store, rpc, &name, address).await?;
        let shape = if shape == NO_VIEW { None } else { Some(shape) };
        rows.push(RegistryFound { id, address, shape });
    }
    Ok(rows)
}

fn probe_view(rpc: &Rpc, factory: Address) -> eyre::Result<Option<usize>> {
    for index in 0..lists::view_count() {
        let selector = lists::view_probe(index);
        match rpc.eth_call(&selector, Some(factory), CALL_GAS) {
            Ok(call) if !call.reverted && call.bytes.len() == 32 => return Ok(Some(index)),
            Ok(_) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

fn search_deploy(rpc: &Rpc, address: Address) -> eyre::Result<Option<u64>> {
    let head = rpc.block_number()?;
    first_code(head, |block| Ok(rpc.code_len(address, Some(block))? > 0))
}

pub(crate) fn first_code(
    head: u64,
    mut has_code: impl FnMut(u64) -> eyre::Result<bool>,
) -> eyre::Result<Option<u64>> {
    if !has_code(head)? {
        return Ok(None);
    }
    let mut lo = 0_u64;
    let mut hi = head;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if has_code(mid)? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(Some(lo))
}

fn read_registries(rpc: &Rpc, provider: Address) -> eyre::Result<Vec<(u64, Address)>> {
    let selector = registry_selector();
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for id in 0..32_u64 {
        let mut data = Vec::with_capacity(36);
        data.extend_from_slice(&selector);
        data.extend_from_slice(&word(id));
        let call = rpc.eth_call(&data, Some(provider), CALL_GAS)?;
        if call.reverted || call.bytes.len() != 32 || call.bytes[..12].iter().any(|byte| *byte != 0)
        {
            continue;
        }
        let address = Address::from_slice(&call.bytes[12..]);
        if address == Address::ZERO || !seen.insert(address) {
            continue;
        }
        found.push((id, address));
    }
    Ok(found)
}

fn registry_selector() -> [u8; 4] {
    let hash = keccak256("get_address(uint256)");
    let mut selector = [0_u8; 4];
    selector.copy_from_slice(&hash[..4]);
    selector
}

fn read_erc20(rpc: &Rpc, token: Address) -> eyre::Result<(String, Option<u8>)> {
    let decimals_call = rpc.eth_call(&SEL_DECIMALS, Some(token), CALL_GAS)?;
    let symbol_call = rpc.eth_call(&SEL_SYMBOL, Some(token), CALL_GAS)?;
    let decimals = if !decimals_call.reverted && decimals_call.bytes.len() == 32 {
        Some(decimals_call.bytes[31])
    } else {
        None
    };
    let symbol = if symbol_call.reverted {
        String::new()
    } else {
        decode_symbol(&symbol_call.bytes)
    };
    Ok((symbol, decimals))
}

fn decode_symbol(bytes: &[u8]) -> String {
    if bytes.len() == 32 && (bytes[0] != 0 || bytes.iter().all(|byte| *byte == 0)) {
        return text_of(bytes);
    }
    if bytes.len() >= 64 {
        if let Some(offset) = word_usize(&bytes[..32]) {
            if offset + 32 <= bytes.len() {
                if let Some(len) = word_usize(&bytes[offset..offset + 32]) {
                    let end = offset + 32 + len;
                    if end <= bytes.len() {
                        return text_of(&bytes[offset + 32..end]);
                    }
                }
            }
        }
    }
    if bytes.len() == 32 {
        return text_of(bytes);
    }
    String::new()
}

fn text_of(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]);
    text.trim().to_string()
}

fn word_usize(word: &[u8]) -> Option<usize> {
    if word.len() < 32 || word[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    let value = u64::from_be_bytes(word[24..32].try_into().ok()?);
    usize::try_from(value).ok()
}

fn word(value: u64) -> [u8; 32] {
    let mut out = [0_u8; 32];
    out[24..].copy_from_slice(&value.to_be_bytes());
    out
}

fn view_of(shape: u64) -> Option<usize> {
    if shape == 0 || shape == NO_VIEW {
        return None;
    }
    let index = (shape - 1) as usize;
    (index < lists::view_count()).then_some(index)
}

fn shape_key(name: &str) -> String {
    format!("shape:list:{name}")
}

fn deploy_key(name: &str) -> String {
    format!("deploy:logs:{name}")
}

fn list_name(name: &str) -> String {
    format!("list:{name}")
}

fn log_name(name: &str) -> String {
    format!("logs:{name}")
}

async fn block_on<T: Send + 'static>(
    work: impl FnOnce() -> eyre::Result<T> + Send + 'static,
) -> eyre::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| eyre!("seed task: {err}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn the_first_block_with_code_is_the_deploy_block() {
        let has = |block: u64| Ok(block >= 100);
        assert_eq!(first_code(1000, has).unwrap(), Some(100));
        let none = |_: u64| Ok(false);
        assert_eq!(first_code(1000, none).unwrap(), None);
        let zero = |_: u64| Ok(true);
        assert_eq!(first_code(0, zero).unwrap(), Some(0));
    }

    #[test]
    fn the_registry_selector_is_get_address() {
        let hash = keccak256("get_address(uint256)");
        assert_eq!(registry_selector().as_slice(), &hash[..4]);
    }

    #[test]
    fn a_symbol_is_a_string_or_a_word() {
        let mut word = [0_u8; 32];
        word[..4].copy_from_slice(b"WETH");
        assert_eq!(decode_symbol(&word), "WETH");
        let mut abi = vec![0_u8; 96];
        abi[31] = 32;
        abi[63] = 4;
        abi[64..68].copy_from_slice(b"USDC");
        assert_eq!(decode_symbol(&abi), "USDC");
    }

    #[test]
    fn factory_roots_merge_and_a_known_registry_is_skipped() {
        let doc = seeds::document();
        let mut shapes = HashMap::new();
        for root in &doc.roots {
            if root.list {
                let view = if root.name == "metric" {
                    3
                } else if root.kind == CURVE {
                    4
                } else {
                    1
                };
                shapes.insert(root.name.clone(), view);
            }
        }
        let meta = doc.by_name("curve-meta").address;
        let fresh = Address::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let quiet = Address::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let registries = vec![
            RegistryFound {
                id: 7,
                address: meta,
                shape: Some(4),
            },
            RegistryFound {
                id: 0,
                address: fresh,
                shape: Some(4),
            },
            RegistryFound {
                id: 1,
                address: quiet,
                shape: None,
            },
        ];
        let plan = assemble(doc, &shapes, &HashMap::new(), &registries);
        assert!(plan.lists.iter().any(|job| job.name == "list:curve-ng"));
        assert!(plan.lists.iter().any(|job| job.name == "list:metric"));
        assert!(plan.lists.iter().any(|job| job.name == "list:uni-v2"));
        assert!(!plan.lists.iter().any(|job| job.name == "list:uni-v3"));
        assert!(!plan
            .lists
            .iter()
            .any(|job| job.name.contains("curve-provider")));
        assert!(!plan.lists.iter().any(|job| job.name == "list:curve-id-7"));
        assert!(plan.lists.iter().any(|job| job.name == "list:curve-id-0"));
        assert!(!plan.lists.iter().any(|job| job.name == "list:curve-id-1"));
        assert!(plan
            .logs
            .iter()
            .any(|job| job.name == "logs:uni-v3" && job.deploy_block == 0));
        assert!(plan.logs.iter().any(|job| job.name == "logs:metric"));
        assert!(!plan.logs.iter().any(|job| job.name == "logs:uni-v2"));
    }
}
