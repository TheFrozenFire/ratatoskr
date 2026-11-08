//! Portable Ethereum JSON-RPC.
//!
//! A lens call omits `to` and passes creation code in `data`. A revert whose
//! `error.data` carries the lens magic is a successful read.

use alloy_primitives::{Address, U256};
use eyre::{eyre, WrapErr};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Duration;

/// Suggested cap from the `eth_getStorageValues` execution-api draft.
const STORAGE_CHUNK: usize = 1024;

pub fn default_call_rpcs() -> Vec<String> {
    vec![
        "https://ethereum-rpc.publicnode.com".into(),
        "https://eth.drpc.org".into(),
        "https://rpc.mevblocker.io".into(),
        "https://rpc.flashbots.net".into(),
    ]
}

#[derive(Debug, Clone)]
pub struct RpcResponse {
    pub http_status: u16,
    pub result: Option<Value>,
    pub error_code: Option<i64>,
    pub error_message: String,
    pub error_data: Option<Vec<u8>>,
}

impl RpcResponse {
    pub fn rotate(&self) -> bool {
        if self.http_status == 429 || self.http_status == 502 || self.http_status == 503 {
            return true;
        }
        matches!(self.error_code, Some(429 | -32005 | -32016))
            || self.error_message.to_lowercase().contains("rate limit")
            || self
                .error_message
                .to_lowercase()
                .contains("too many request")
    }
}

#[derive(Debug)]
pub struct CallBytes {
    pub bytes: Vec<u8>,
    pub reverted: bool,
    pub message: String,
}

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::blocking::Client,
    urls: Vec<String>,
    next: Cell<usize>,
    id: Cell<u64>,
    /// Cleared after a provider rejects `eth_getStorageValues`. Later reads skip the method.
    prefer_storage_values: Cell<bool>,
}

impl Rpc {
    pub fn new(urls: Vec<String>) -> eyre::Result<Self> {
        if urls.is_empty() {
            return Err(eyre!("no rpc url"));
        }
        let client = reqwest::blocking::Client::builder()
            .user_agent("ratatoskr-venues/0.1")
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .wrap_err("rpc client")?;
        Ok(Self {
            client,
            urls,
            next: Cell::new(0),
            id: Cell::new(1),
            prefer_storage_values: Cell::new(true),
        })
    }

    /// Slot values in request order, via `eth_getStorageValues`.
    ///
    /// A provider that does not implement the method returns [`StorageError::Unsupported`].
    /// Callers then read those words with the tick lens. `eth_getStorageAt` is not a fallback:
    /// each call is one word, one compute unit, and a separate state lookup.
    pub fn storage_words(&self, reads: &[(Address, U256)]) -> Result<Vec<U256>, StorageError> {
        self.storage_words_at(reads, None)
    }

    /// Same read as [`Self::storage_words`], at one block.
    ///
    /// `None` is the tag `latest`. A generation's detail call and tick call
    /// pass the same block so the bitmap and the tick words match.
    pub fn storage_words_at(
        &self,
        reads: &[(Address, U256)],
        block: Option<u64>,
    ) -> Result<Vec<U256>, StorageError> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        if !self.prefer_storage_values.get() {
            return Err(StorageError::Unsupported);
        }
        let mut out = Vec::with_capacity(reads.len());
        for chunk in reads.chunks(STORAGE_CHUNK) {
            match self.storage_values_call(chunk, block) {
                Ok(words) => out.extend(words),
                Err(StorageError::Unsupported) => {
                    self.prefer_storage_values.set(false);
                    return Err(StorageError::Unsupported);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(out)
    }

    fn storage_values_call(
        &self,
        chunk: &[(Address, U256)],
        block: Option<u64>,
    ) -> Result<Vec<U256>, StorageError> {
        let mut grouped: BTreeMap<Address, Vec<U256>> = BTreeMap::new();
        for (account, slot) in chunk {
            grouped.entry(*account).or_default().push(*slot);
        }
        let mut map = serde_json::Map::new();
        for (account, slots) in &grouped {
            let hexes = slots
                .iter()
                .map(|slot| format!("{slot:#x}"))
                .collect::<Vec<_>>();
            map.insert(account.to_string(), json!(hexes));
        }
        let response = self
            .exchange("eth_getStorageValues", json!([map, block_tag(block)]))
            .map_err(StorageError::Failed)?;
        if values_unsupported(&response) {
            return Err(StorageError::Unsupported);
        }
        let Some(result) = response.result.as_ref() else {
            return Err(StorageError::Failed(eyre!(
                "eth_getStorageValues: {}",
                response.error_message
            )));
        };
        let parsed = values_by_address(result).map_err(StorageError::Failed)?;
        let mut out = Vec::with_capacity(chunk.len());
        let mut cursors: BTreeMap<Address, usize> = BTreeMap::new();
        for (account, _) in chunk {
            let words = parsed.get(account).ok_or_else(|| {
                StorageError::Failed(eyre!("eth_getStorageValues omitted {account}"))
            })?;
            let cursor = cursors.entry(*account).or_insert(0);
            let word = words.get(*cursor).copied().ok_or_else(|| {
                StorageError::Failed(eyre!("eth_getStorageValues short for {account}"))
            })?;
            *cursor += 1;
            out.push(word);
        }
        Ok(out)
    }

    /// `to` is omitted for a creation-code lens. The block tag is `latest`.
    pub fn eth_call(&self, data: &[u8], to: Option<Address>, gas: u64) -> eyre::Result<CallBytes> {
        self.eth_call_at(data, to, gas, None)
    }

    /// `to` is omitted for a creation-code lens.
    pub fn eth_call_at(
        &self,
        data: &[u8],
        to: Option<Address>,
        gas: u64,
        block: Option<u64>,
    ) -> eyre::Result<CallBytes> {
        let mut tx = serde_json::Map::new();
        tx.insert("data".into(), json!(hex_0x(data)));
        tx.insert("gas".into(), json!(format!("0x{gas:x}")));
        if let Some(to) = to {
            tx.insert("to".into(), json!(to.to_string()));
        }
        let response = self.exchange("eth_call", json!([tx, block_tag(block)]))?;
        call_bytes(response)
    }

    pub fn call_word(&self, to: Address, data: &[u8]) -> eyre::Result<U256> {
        let call = self.eth_call(data, Some(to), 1_000_000)?;
        if call.reverted || call.bytes.len() != 32 {
            return Err(eyre!("view {} reverted: {}", to, call.message));
        }
        Ok(U256::from_be_slice(&call.bytes))
    }

    pub fn block_number(&self) -> eyre::Result<u64> {
        let response = self.exchange("eth_blockNumber", json!([]))?;
        let Some(text) = response.result.as_ref().and_then(Value::as_str) else {
            return Err(eyre!("block number"));
        };
        let hex = text.strip_prefix("0x").unwrap_or(text);
        u64::from_str_radix(hex, 16).wrap_err("block number")
    }

    fn exchange(&self, method: &str, params: Value) -> eyre::Result<RpcResponse> {
        let body = self.body(method, params);
        let start = self.next.get();
        let mut last = None;
        for step in 0..self.urls.len() {
            let index = (start + step) % self.urls.len();
            let url = &self.urls[index];
            for attempt in 0..5 {
                match self.post(url, &body) {
                    Ok(response) if response.rotate() => {
                        tracing::warn!(
                            endpoint = %redact_secret(url),
                            status = response.http_status,
                            error = %response.error_message,
                            attempt,
                            "rpc rotate"
                        );
                        last = Some(eyre::eyre!(
                            "{}: {}",
                            redact_secret(url),
                            response.error_message
                        ));
                        if attempt + 1 < 5 {
                            std::thread::sleep(Duration::from_millis(500 << attempt));
                        }
                    }
                    Ok(response) => {
                        self.next.set(index);
                        return Ok(response);
                    }
                    Err(err) => {
                        let shown = redact_secret(&format!("{err:#}"));
                        tracing::warn!(
                            endpoint = %redact_secret(url),
                            error = %shown,
                            attempt,
                            "rpc transport"
                        );
                        last = Some(eyre::eyre!("{shown}"));
                        if attempt + 1 < 5 {
                            std::thread::sleep(Duration::from_millis(500 << attempt));
                        }
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| eyre!("rpc {method} failed")))
    }

    fn body(&self, method: &str, params: Value) -> Value {
        let id = self.id.get();
        self.id.set(id.wrapping_add(1));
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn post(&self, url: &str, body: &Value) -> eyre::Result<RpcResponse> {
        let response = self
            .client
            .post(url)
            .json(body)
            .send()
            .wrap_err_with(|| format!("post {url}"))?;
        let status = response.status().as_u16();
        let text = response.text().wrap_err_with(|| format!("read {url}"))?;
        parse_response(status, &text).wrap_err_with(|| format!("{url} http {status}"))
    }
}

#[derive(Debug)]
pub enum StorageError {
    Unsupported,
    Failed(eyre::Report),
}

fn block_tag(block: Option<u64>) -> Value {
    match block {
        Some(number) => json!(format!("0x{number:x}")),
        None => json!("latest"),
    }
}

fn values_unsupported(response: &RpcResponse) -> bool {
    let message = response.error_message.to_lowercase();
    message.contains("unsupported method")
        || message.contains("method not found")
        || message.contains("method does not exist")
        || response.error_code == Some(-32601)
}

fn values_by_address(result: &Value) -> eyre::Result<BTreeMap<Address, Vec<U256>>> {
    let Value::Object(map) = result else {
        return Err(eyre!("storage values result"));
    };
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let account: Address = key.parse().map_err(|_| eyre!("storage address {key}"))?;
        let Value::Array(words) = value else {
            return Err(eyre!("storage values for {account}"));
        };
        let mut parsed = Vec::with_capacity(words.len());
        for word in words {
            let Value::String(text) = word else {
                return Err(eyre!("storage word"));
            };
            parsed.push(word_u256(text)?);
        }
        out.insert(account, parsed);
    }
    Ok(out)
}

fn word_u256(text: &str) -> eyre::Result<U256> {
    let bytes = unhex(text)?;
    if bytes.len() != 32 {
        return Err(eyre!("storage word width {}", bytes.len()));
    }
    Ok(U256::from_be_slice(&bytes))
}

fn call_bytes(response: RpcResponse) -> eyre::Result<CallBytes> {
    if let Some(data) = response.error_data.clone() {
        return Ok(CallBytes {
            bytes: data,
            reverted: true,
            message: response.error_message,
        });
    }
    if let Some(Value::String(text)) = &response.result {
        return Ok(CallBytes {
            bytes: unhex(text).wrap_err("eth_call result")?,
            reverted: false,
            message: response.error_message,
        });
    }
    if response.error_code.is_some() || response.http_status >= 400 {
        return Ok(CallBytes {
            bytes: Vec::new(),
            reverted: true,
            message: if response.error_message.is_empty() {
                format!("http {}", response.http_status)
            } else {
                response.error_message
            },
        });
    }
    Err(eyre!("eth_call returned no result"))
}

fn parse_response(status: u16, text: &str) -> eyre::Result<RpcResponse> {
    let body: Value = serde_json::from_str(text).wrap_err("rpc json")?;
    let error = body.get("error");
    let error_code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_i64);
    let error_message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let error_data = error
        .and_then(|value| value.get("data"))
        .and_then(data_bytes);
    Ok(RpcResponse {
        http_status: status,
        result: body.get("result").cloned(),
        error_code,
        error_message,
        error_data,
    })
}

fn data_bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::String(text) => unhex(text).ok(),
        Value::Object(map) => map
            .get("data")
            .and_then(Value::as_str)
            .and_then(|text| unhex(text).ok()),
        _ => None,
    }
}

pub fn hex_0x(bytes: &[u8]) -> String {
    format!("0x{}", alloy_primitives::hex::encode(bytes))
}

pub(crate) fn redact_secret(text: &str) -> String {
    let marker = "eth-mainnet.g.alchemy.com/v2/";
    let mut out = text.to_string();
    let mut search = 0;
    while let Some(relative) = out[search..].find(marker) {
        let from = search + relative + marker.len();
        let end = out[from..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .map(|index| from + index)
            .unwrap_or(out.len());
        if from == end {
            break;
        }
        out.replace_range(from..end, "redacted");
        search = from + "redacted".len();
    }
    out
}

pub fn unhex(text: &str) -> eyre::Result<Vec<u8>> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    if !text.len().is_multiple_of(2) {
        return Err(eyre!("odd hex"));
    }
    alloy_primitives::hex::decode(text).map_err(|err| eyre!(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_values_keep_slot_order_per_address() {
        let text = r#"{"0x00000000000000000000000000000000000000aa":["0x0000000000000000000000000000000000000000000000000000000000000001","0x0000000000000000000000000000000000000000000000000000000000000002"],"0x00000000000000000000000000000000000000bb":["0x0000000000000000000000000000000000000000000000000000000000000003"]}"#;
        let parsed = values_by_address(&serde_json::from_str(text).unwrap()).unwrap();
        let aa: Address = "0x00000000000000000000000000000000000000aa"
            .parse()
            .unwrap();
        assert_eq!(parsed[&aa][1], U256::from(2));
    }

    #[test]
    fn alchemy_rejects_storage_values_as_unsupported() {
        let response = RpcResponse {
            http_status: 200,
            result: None,
            error_code: Some(-32600),
            error_message: "Unsupported method: eth_getStorageValues on ETH_MAINNET".into(),
            error_data: None,
        };
        assert!(values_unsupported(&response));
    }

    #[test]
    fn redact_strips_an_alchemy_key() {
        let text = "post https://eth-mainnet.g.alchemy.com/v2/secretkeyvalue: timeout";
        let out = redact_secret(text);
        assert!(!out.contains("secretkeyvalue"), "{out}");
        assert!(out.contains("redacted"), "{out}");
    }

    #[test]
    fn error_data_object_is_the_revert_payload() {
        let text = r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted","data":"0xabcd"}}"#;
        let parsed = parse_response(200, text).unwrap();
        assert_eq!(parsed.error_code, Some(3));
        assert_eq!(parsed.error_data.unwrap(), vec![0xab, 0xcd]);
    }
}
