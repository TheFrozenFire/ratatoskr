//! Directory rows and proxy reads, against fixture bytes.

use alloy_primitives::{address, Address, I256, U256};
use oracles::{
    decode_aggregate3, decode_bound, decode_proxy_head, proxy_heads, Configuration, Mismatch,
    ProxyHead, Read, DIRECTORY_URL,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use store::service::Feeds;

fn rows(body: &str) -> Vec<store_model::DirectoryRow> {
    oracles::parse_directory(body).unwrap()
}

const ETH: Address = address!("5f4eC3Df9cbd43714FE2740f5E3616155c5b8419");
const ETH_AGG: Address = address!("E62B71cf983019BFf55bC83B48601ce8419650CC");
const SVR: Address = address!("5c9C449BbC9a6075A2c061dF312a35fd1E05fF22");
const SVR_SECONDARY: Address = address!("7d4E742018fb52E48b08BE73d041C18B21de6Fb5");
const SVR_AGG: Address = address!("1111111111111111111111111111111111111111");
const QUIET: Address = address!("2222222222222222222222222222222222222222");
const QUIET_AGG: Address = address!("3333333333333333333333333333333333333333");

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "oracles-ingest-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open() -> (TempDir, Feeds) {
    let dir = TempDir::new();
    let feeds = Feeds::open(&dir.0.join("feeds.redb")).unwrap();
    (dir, feeds)
}

fn directory() -> String {
    format!(
        r#"[
            {{
                "proxyAddress": "{eth}",
                "name": "ETH / USD",
                "path": "eth-usd",
                "feedCategory": "low",
                "feedType": "crypto",
                "contractVersion": 6,
                "decimals": 8,
                "multiply": "100000000",
                "heartbeat": 3600,
                "threshold": 0.5,
                "contractAddress": "{eth_agg}",
                "docs": {{
                    "baseAsset": "ETH",
                    "quoteAsset": "USD",
                    "productType": "RefPrice"
                }}
            }},
            {{
                "proxyAddress": "{svr}",
                "name": "ETH / USD",
                "path": "eth-usd-svr",
                "feedCategory": "medium",
                "feedType": "crypto",
                "contractVersion": 6,
                "decimals": 8,
                "multiply": "100000000",
                "heartbeat": 86400,
                "threshold": 1,
                "contractAddress": "{svr_agg}",
                "secondaryProxyAddress": "{secondary}",
                "svrDisplayLabel": "Aave SVR",
                "docs": {{
                    "baseAsset": "ETH",
                    "quoteAsset": "USD",
                    "clicProductName": "ETH / USD"
                }}
            }},
            {{
                "proxyAddress": "{quiet}",
                "name": "SUSD / ETH",
                "path": "susd-eth",
                "feedCategory": "",
                "feedType": "crypto",
                "contractVersion": 6,
                "decimals": 18,
                "multiply": "1000000000000000000",
                "heartbeat": 86400,
                "threshold": 0.5,
                "contractAddress": "{quiet_agg}",
                "docs": {{
                    "shutdownDate": "2024-10-31",
                    "baseAsset": "SUSD",
                    "quoteAsset": "ETH"
                }}
            }}
        ]"#,
        eth = ETH,
        eth_agg = ETH_AGG,
        svr = SVR,
        svr_agg = SVR_AGG,
        secondary = SVR_SECONDARY,
        quiet = QUIET,
        quiet_agg = QUIET_AGG,
    )
}

fn head(decimals: u8, phase_id: u16, aggregator: Address) -> ProxyHead {
    ProxyHead {
        decimals,
        description: "ETH / USD".into(),
        version: U256::from(6u8),
        aggregator,
        phase_id,
    }
}

fn configured(head: ProxyHead, min_answer: Option<I256>, max_answer: Option<I256>) -> Read {
    Read::Configured {
        head,
        min_answer,
        max_answer,
    }
}

fn word(value: u64) -> Vec<u8> {
    U256::from(value).to_be_bytes::<32>().to_vec()
}

fn address_word(address: Address) -> Vec<u8> {
    let mut out = vec![0u8; 32];
    out[12..].copy_from_slice(address.as_slice());
    out
}

fn abi_string(text: &str) -> Vec<u8> {
    let mut out = word(32);
    out.extend(word(text.len() as u64));
    out.extend_from_slice(text.as_bytes());
    let pad = (32 - (text.len() % 32)) % 32;
    out.extend(std::iter::repeat(0).take(pad));
    out
}

fn encode_results(items: &[(bool, Vec<u8>)]) -> Vec<u8> {
    let mut elements = Vec::new();
    for (ok, body) in items {
        let mut element = word(u64::from(*ok));
        element.extend(word(0x40));
        element.extend(word(body.len() as u64));
        element.extend_from_slice(body);
        let pad = (32 - (body.len() % 32)) % 32;
        element.extend(std::iter::repeat(0).take(pad));
        elements.push(element);
    }
    let mut out = word(0x20);
    out.extend(word(items.len() as u64));
    let mut cursor = items.len() * 32;
    let mut tails = Vec::new();
    for element in &elements {
        out.extend(word(cursor as u64));
        cursor += element.len();
        tails.extend_from_slice(element);
    }
    out.extend(tails);
    out
}

#[test]
fn the_directory_url_is_the_ethereum_mainnet_file() {
    assert_eq!(
        DIRECTORY_URL,
        "https://reference-data-directory.vercel.app/feeds-mainnet.json"
    );
}

#[test]
fn directory_rows_keep_the_proxy_as_the_identity() {
    let (_dir, feeds) = open();
    let listing = feeds.apply_directory(&rows(&directory())).unwrap();
    assert_eq!(listing.generation, 1);
    assert_eq!(listing.named, 3);
    let eth = feeds.feed(ETH).unwrap().unwrap();
    assert_eq!(eth.row.proxy, ETH);
    assert_eq!(eth.row.category, "low");
    assert_eq!(eth.row.threshold, "0.5");
    assert_eq!(eth.row.path, "eth-usd");
    assert_eq!(eth.row.directory_aggregator, ETH_AGG);
    assert_eq!(eth.listed_generation, 1);
    let svr = feeds.feed(SVR).unwrap().unwrap();
    assert_eq!(svr.row.secondary_proxy, Some(SVR_SECONDARY));
    assert_eq!(svr.row.svr_label, "Aave SVR");
    assert_eq!(svr.row.clic_product_name, "ETH / USD");
    let quiet = feeds.feed(QUIET).unwrap().unwrap();
    assert_eq!(quiet.row.category, "");
    assert_eq!(quiet.row.shutdown_date, "2024-10-31");
}

#[test]
fn a_later_directory_rewrites_category_and_keeps_the_proxy_configuration() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(
            ETH,
            configured(
                head(8, 7, ETH_AGG),
                Some(I256::MINUS_ONE),
                Some(I256::try_from(1).unwrap()),
            ),
        )
        .unwrap();
    let rewritten =
        directory().replacen(r#""feedCategory": "low""#, r#""feedCategory": "medium""#, 1);
    let listing = feeds.apply_directory(&rows(&rewritten)).unwrap();
    assert_eq!(listing.generation, 2);
    let eth = feeds.feed(ETH).unwrap().unwrap();
    assert_eq!(eth.row.proxy, ETH);
    assert_eq!(eth.row.category, "medium");
    assert_eq!(eth.listed_generation, 2);
    assert_eq!(
        feeds.configuration(ETH).unwrap(),
        Some(Configuration {
            decimals: 8,
            description: "ETH / USD".into(),
            version: U256::from(6u8),
        })
    );
    assert_eq!(feeds.phases(ETH).unwrap().len(), 1);
    assert_eq!(feeds.history_jobs().unwrap().len(), 1);
}

#[test]
fn an_omitted_proxy_keeps_its_listing_generation() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    let only_eth = format!(
        r#"[{{
            "proxyAddress": "{ETH}",
            "name": "ETH / USD",
            "path": "eth-usd",
            "feedCategory": "low",
            "feedType": "crypto",
            "contractVersion": 6,
            "decimals": 8,
            "multiply": "100000000",
            "heartbeat": 3600,
            "threshold": 0.5,
            "contractAddress": "{ETH_AGG}"
        }}]"#
    );
    let listing = feeds.apply_directory(&rows(&only_eth)).unwrap();
    assert_eq!(listing.generation, 2);
    assert_eq!(listing.named, 1);
    assert_eq!(feeds.listing_generation().unwrap(), 2);
    assert_eq!(feeds.feed(SVR).unwrap().unwrap().listed_generation, 1);
    assert_eq!(feeds.feed(ETH).unwrap().unwrap().listed_generation, 2);
    assert!(feeds.feed(QUIET).unwrap().is_some());
}

#[test]
fn a_repeated_proxy_does_not_apply() {
    let (_dir, feeds) = open();
    let body = format!(
        r#"[
            {{"proxyAddress":"{ETH}","name":"ETH / USD","path":"eth-usd","feedCategory":"low","feedType":"crypto","contractVersion":6,"decimals":8,"multiply":"100000000","heartbeat":3600,"threshold":0.5,"contractAddress":"{ETH_AGG}"}},
            {{"proxyAddress":"{ETH}","name":"ETH / USD","path":"eth-usd-again","feedCategory":"high","feedType":"crypto","contractVersion":6,"decimals":8,"multiply":"100000000","heartbeat":3600,"threshold":0.5,"contractAddress":"{ETH_AGG}"}}
        ]"#
    );
    let err = oracles::parse_directory(&body).unwrap_err();
    assert!(err.to_string().contains("repeated"), "{err}");
    assert_eq!(feeds.listing_generation().unwrap(), 0);
    assert!(feeds.proxies().unwrap().is_empty());
}

#[test]
fn a_missing_proxy_and_a_bad_threshold_are_errors() {
    let missing = oracles::parse_directory(r#"[{"name":"ETH / USD"}]"#).unwrap_err();
    assert!(missing.to_string().contains("proxy"), "{missing}");
    let threshold = oracles::parse_directory(&format!(
        r#"[{{"proxyAddress":"{ETH}","name":"ETH / USD","path":"eth-usd","feedCategory":"low","feedType":"crypto","contractVersion":6,"decimals":8,"multiply":"100000000","heartbeat":3600,"threshold":"0.5","contractAddress":"{ETH_AGG}"}}]"#
    ))
    .unwrap_err();
    assert!(threshold.to_string().contains("threshold"), "{threshold}");
}

#[test]
fn a_proxy_read_stores_the_chain_phase_and_one_history_job() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    let description = abi_string("ETH / USD");
    let results = encode_results(&[
        (true, word(8)),
        (true, description),
        (true, word(6)),
        (true, address_word(ETH_AGG)),
        (true, word(7)),
        (true, word(1)),
        (false, Vec::new()),
    ]);
    let decoded = decode_aggregate3(&results).unwrap();
    let heads = proxy_heads(&[ETH], &decoded[..5]).unwrap();
    let head = heads[0].1.clone().unwrap();
    assert_eq!(head.phase_id, 7);
    assert_eq!(head.decimals, 8);
    assert_eq!(head.description, "ETH / USD");
    assert_eq!(head.aggregator, ETH_AGG);
    let min_answer = decode_bound(&decoded[5]);
    let max_answer = decode_bound(&decoded[6]);
    assert_eq!(min_answer, Some(I256::try_from(1).unwrap()));
    assert_eq!(max_answer, None);
    feeds
        .apply_read(ETH, configured(head, min_answer, max_answer))
        .unwrap();
    let phases = feeds.phases(ETH).unwrap();
    assert_eq!(phases.len(), 1);
    assert_eq!(phases[0].phase_id, 7);
    assert_eq!(phases[0].min_answer, Some(I256::try_from(1).unwrap()));
    assert_eq!(phases[0].max_answer, None);
    let jobs = feeds.history_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].proxy, ETH);
    assert_eq!(jobs[0].phase_id, 7);
    assert_eq!(jobs[0].aggregator, ETH_AGG);
    assert!(feeds.failure(ETH).unwrap().is_none());
}

#[test]
fn the_same_phase_does_not_add_a_job() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    let bounds = Some(I256::try_from(5).unwrap());
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), bounds, bounds))
        .unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    assert_eq!(feeds.history_jobs().unwrap().len(), 1);
    let phase = &feeds.phases(ETH).unwrap()[0];
    assert_eq!(phase.min_answer, bounds);
    assert_eq!(phase.max_answer, bounds);
    assert!(feeds.mismatches(ETH).unwrap().is_empty());
}

#[test]
fn a_greater_phase_appends_one_phase_and_one_job() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    let next = address!("4444444444444444444444444444444444444444");
    feeds
        .apply_read(ETH, configured(head(8, 9, next), None, None))
        .unwrap();
    let phases = feeds.phases(ETH).unwrap();
    assert_eq!(phases.len(), 2);
    assert_eq!(phases[0].phase_id, 7);
    assert_eq!(phases[1].phase_id, 9);
    assert_eq!(phases[1].aggregator, next);
    assert_eq!(feeds.history_jobs().unwrap().len(), 2);
}

#[test]
fn a_smaller_phase_is_a_mismatch() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 4, ETH_AGG), None, None))
        .unwrap();
    assert_eq!(feeds.phases(ETH).unwrap().len(), 1);
    assert_eq!(feeds.history_jobs().unwrap().len(), 1);
    assert_eq!(
        feeds.mismatches(ETH).unwrap(),
        vec![Mismatch::Phase {
            newest: 7,
            observed: 4,
        }]
    );
}

#[test]
fn a_changed_aggregator_on_the_same_phase_is_a_mismatch() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    let other = address!("5555555555555555555555555555555555555555");
    feeds
        .apply_read(ETH, configured(head(8, 7, other), None, None))
        .unwrap();
    let phases = feeds.phases(ETH).unwrap();
    assert_eq!(phases.len(), 1);
    assert_eq!(phases[0].aggregator, ETH_AGG);
    assert_eq!(feeds.history_jobs().unwrap().len(), 1);
    assert_eq!(feeds.history_jobs().unwrap()[0].aggregator, ETH_AGG);
    assert_eq!(
        feeds.mismatches(ETH).unwrap(),
        vec![Mismatch::Aggregator {
            phase_id: 7,
            stored: ETH_AGG,
            observed: other,
        }]
    );
}

#[test]
fn disagreeing_decimals_stay_at_the_first_read() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    let mut later = head(18, 8, ETH_AGG);
    later.description = "ETH / USD later".into();
    later.version = U256::from(7u8);
    feeds
        .apply_read(ETH, configured(later, None, None))
        .unwrap();
    assert_eq!(feeds.configuration(ETH).unwrap().unwrap().decimals, 8);
    assert_eq!(
        feeds.configuration(ETH).unwrap().unwrap().description,
        "ETH / USD"
    );
    assert_eq!(feeds.phases(ETH).unwrap().len(), 2);
    assert_eq!(feeds.history_jobs().unwrap().len(), 2);
    match &feeds.mismatches(ETH).unwrap()[0] {
        Mismatch::Configuration {
            stored_decimals,
            observed_decimals,
            stored_description,
            observed_description,
            stored_version,
            observed_version,
        } => {
            assert_eq!(*stored_decimals, 8);
            assert_eq!(*observed_decimals, 18);
            assert_eq!(stored_description, "ETH / USD");
            assert_eq!(observed_description, "ETH / USD later");
            assert_eq!(*stored_version, U256::from(6u8));
            assert_eq!(*observed_version, U256::from(7u8));
        }
        other => panic!("configuration mismatch, got {other:?}"),
    }
}

#[test]
fn a_reverted_proxy_stores_a_failure_and_no_job() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    let body = encode_results(&[
        (false, Vec::new()),
        (true, abi_string("ETH / USD")),
        (true, word(6)),
        (true, address_word(ETH_AGG)),
        (true, word(7)),
    ]);
    let decoded = decode_aggregate3(&body).unwrap();
    let err = decode_proxy_head(&decoded).unwrap_err();
    assert!(err.contains("decimals"), "{err}");
    feeds.apply_read(ETH, Read::Failed(err.clone())).unwrap();
    assert_eq!(feeds.failure(ETH).unwrap().as_deref(), Some(err.as_str()));
    assert!(feeds.phases(ETH).unwrap().is_empty());
    assert!(feeds.history_jobs().unwrap().is_empty());
    assert!(feeds.configuration(ETH).unwrap().is_none());
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    assert!(feeds.failure(ETH).unwrap().is_none());
    assert_eq!(feeds.phases(ETH).unwrap().len(), 1);
    assert_eq!(feeds.history_jobs().unwrap().len(), 1);
}

#[test]
fn a_later_revert_keeps_phases_and_jobs() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    feeds
        .apply_read(ETH, Read::Failed("decimals reverted".into()))
        .unwrap();
    assert_eq!(
        feeds.failure(ETH).unwrap().as_deref(),
        Some("decimals reverted")
    );
    assert_eq!(feeds.phases(ETH).unwrap().len(), 1);
    assert_eq!(feeds.history_jobs().unwrap().len(), 1);
    assert!(feeds.configuration(ETH).unwrap().is_some());
}

#[test]
fn clearing_history_jobs_leaves_the_feed_and_the_phase() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 7, ETH_AGG), None, None))
        .unwrap();
    feeds.clear_history_jobs().unwrap();
    assert!(feeds.history_jobs().unwrap().is_empty());
    assert_eq!(feeds.feed(ETH).unwrap().unwrap().row.proxy, ETH);
    assert_eq!(feeds.phases(ETH).unwrap()[0].phase_id, 7);
    assert!(feeds.configuration(ETH).unwrap().is_some());
}

#[test]
fn phase_zero_is_stored_when_the_aggregator_is_present() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    feeds
        .apply_read(ETH, configured(head(8, 0, ETH_AGG), None, None))
        .unwrap();
    assert_eq!(feeds.phases(ETH).unwrap()[0].phase_id, 0);
    assert_eq!(feeds.history_jobs().unwrap()[0].phase_id, 0);
}

#[test]
fn an_empty_aggregator_is_a_proxy_failure() {
    let body = encode_results(&[
        (true, word(8)),
        (true, abi_string("ETH / USD")),
        (true, word(6)),
        (true, address_word(Address::ZERO)),
        (true, word(1)),
    ]);
    let decoded = decode_aggregate3(&body).unwrap();
    let err = decode_proxy_head(&decoded).unwrap_err();
    assert_eq!(err, "aggregator is empty");
}

#[test]
fn an_unknown_proxy_read_is_an_error() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    let err = feeds
        .apply_read(
            address!("6666666666666666666666666666666666666666"),
            configured(head(8, 1, ETH_AGG), None, None),
        )
        .unwrap_err();
    assert!(err.to_string().contains("not listed"), "{err}");
}
