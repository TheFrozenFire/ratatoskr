//! Answers stored from aggregator logs.

use alloy_primitives::{address, Address, B256, I256};
use oracles::{decode_answer_updated, ChainLog, ObservedAnswer, ProxyHead, Read, ANSWER_UPDATED};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use store::service::Feeds;

fn rows(body: &str) -> Vec<store_model::DirectoryRow> {
    oracles::parse_directory(body).unwrap()
}

const ETH: Address = address!("5f4eC3Df9cbd43714FE2740f5E3616155c5b8419");
const ETH_AGG: Address = address!("E62B71cf983019BFf55bC83B48601ce8419650CC");
const NEXT_AGG: Address = address!("4444444444444444444444444444444444444444");

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "oracles-answers-{}-{}",
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
    )
}

fn read(aggregator: Address, phase_id: u16) -> Read {
    Read::Configured {
        head: ProxyHead {
            decimals: 8,
            description: "ETH / USD".into(),
            version: alloy_primitives::U256::from(6u8),
            aggregator,
            phase_id,
        },
        min_answer: None,
        max_answer: None,
    }
}

fn word(value: u64) -> B256 {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&value.to_be_bytes());
    B256::from(out)
}

fn signed(value: i64) -> B256 {
    let mut out = if value < 0 { [0xffu8; 32] } else { [0u8; 32] };
    out[24..].copy_from_slice(&value.to_be_bytes());
    B256::from(out)
}

fn observed(aggregator: Address, round_id: u64, value: i64) -> ObservedAnswer {
    ObservedAnswer {
        aggregator,
        round_id,
        value: I256::try_from(value).unwrap(),
        updated_at: 20,
        block: 12,
        log_index: 0,
    }
}

#[test]
fn an_answer_updated_log_stores_the_aggregator_round() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    oracles::record_read(&feeds, ETH, read(ETH_AGG, 7)).unwrap();
    oracles::record_read(&feeds, ETH, read(NEXT_AGG, 9)).unwrap();
    let log = ChainLog {
        address: NEXT_AGG,
        block: 100,
        log_index: 4,
        topics: vec![ANSWER_UPDATED, signed(1_500), word(42)],
        data: word(1_700_000_000).to_vec(),
    };
    let decoded = decode_answer_updated(&log).unwrap();
    assert_eq!(decoded.round_id, 42);
    assert_eq!(decoded.value, I256::try_from(1_500).unwrap());
    assert_eq!(
        oracles::commit_observed(&feeds, 101, &[decoded]).unwrap(),
        1
    );
    let answers = feeds.answers(ETH).unwrap();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].phase_id, 9);
    assert_eq!(answers[0].round_id, 42);
    assert_eq!(answers[0].updated_at, 1_700_000_000);
    assert_eq!(answers[0].block, 100);
    assert_eq!(feeds.follow_block().unwrap(), Some(101));
    assert_eq!(feeds.gauges().unwrap().answers, 1);
    assert_eq!(
        oracles::commit_observed(&feeds, 101, &[observed(NEXT_AGG, 42, 1_500)]).unwrap(),
        0
    );
    assert_eq!(feeds.answers(ETH).unwrap().len(), 1);
}

#[test]
fn a_zero_log_and_an_unknown_aggregator_store_nothing() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    oracles::record_read(&feeds, ETH, read(ETH_AGG, 7)).unwrap();
    let zero = ChainLog {
        address: ETH_AGG,
        block: 3,
        log_index: 1,
        topics: vec![ANSWER_UPDATED, signed(0), word(1)],
        data: word(10).to_vec(),
    };
    assert!(decode_answer_updated(&zero).is_none());
    let written = oracles::commit_observed(
        &feeds,
        4,
        &[observed(
            address!("5555555555555555555555555555555555555555"),
            1,
            9,
        )],
    )
    .unwrap();
    assert_eq!(written, 0);
    assert!(feeds.answers(ETH).unwrap().is_empty());
}

#[test]
fn an_older_phase_keeps_the_log_from_its_own_aggregator() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    oracles::record_read(&feeds, ETH, read(ETH_AGG, 7)).unwrap();
    oracles::record_read(&feeds, ETH, read(NEXT_AGG, 9)).unwrap();
    oracles::commit_observed(&feeds, 50, &[observed(ETH_AGG, 8, 11)]).unwrap();
    let answers = feeds.answers(ETH).unwrap();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].phase_id, 7);
    assert_eq!(answers[0].round_id, 8);
}

#[test]
fn the_heartbeat_lookback_is_remembered_once() {
    let (_dir, feeds) = open();
    assert!(!feeds.lookback_done().unwrap());
    feeds.mark_lookback().unwrap();
    assert!(feeds.lookback_done().unwrap());
}

#[test]
fn the_follow_cursor_does_not_move_backward() {
    let (_dir, feeds) = open();
    feeds.apply_directory(&rows(&directory())).unwrap();
    oracles::commit_observed(&feeds, 20, &[]).unwrap();
    let err = oracles::commit_observed(&feeds, 19, &[]).unwrap_err();
    assert!(err.to_string().contains("follow cursor"), "{err}");
    assert_eq!(feeds.follow_block().unwrap(), Some(20));
}
