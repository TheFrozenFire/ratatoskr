//! Portable Ethereum JSON-RPC.
//!
//! A lens call omits `to` and passes creation code in `data`. A revert whose
//! `error.data` carries the lens magic is a successful read.

use alloy_primitives::{Address, U256};
use eyre::{eyre, WrapErr};
use serde_json::{json, Value};
use std::cell::Cell;
use std::time::Duration;

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
        })
    }

    /// `to` is omitted for a creation-code lens.
    pub fn eth_call(&self, data: &[u8], to: Option<Address>, gas: u64) -> eyre::Result<CallBytes> {
        let mut tx = serde_json::Map::new();
        tx.insert("data".into(), json!(hex_0x(data)));
        tx.insert("gas".into(), json!(format!("0x{gas:x}")));
        if let Some(to) = to {
            tx.insert("to".into(), json!(to.to_string()));
        }
        let response = self.exchange("eth_call", json!([tx, "latest"]))?;
        call_bytes(response)
    }

    pub fn call_word(&self, to: Address, data: &[u8]) -> eyre::Result<U256> {
        let call = self.eth_call(data, Some(to), 1_000_000)?;
        if call.reverted || call.bytes.len() != 32 {
            return Err(eyre!("view {} reverted: {}", to, call.message));
        }
        Ok(U256::from_be_slice(&call.bytes))
    }

    fn exchange(&self, method: &str, params: Value) -> eyre::Result<RpcResponse> {
        let body = self.body(method, params);
        let start = self.next.get();
        let mut last = None;
        for step in 0..self.urls.len() {
            let index = (start + step) % self.urls.len();
            let url = &self.urls[index];
            match self.post(url, &body) {
                Ok(response) if response.rotate() => {
                    tracing::warn!(url, status = response.http_status, error = %response.error_message, "rpc rotate");
                    last = Some(eyre!("{}: {}", url, response.error_message));
                    std::thread::sleep(Duration::from_millis(400 * (step as u64 + 1)));
                }
                Ok(response) => {
                    self.next.set(index);
                    return Ok(response);
                }
                Err(err) => {
                    tracing::warn!(url, error = %err, "rpc transport");
                    last = Some(err);
                    std::thread::sleep(Duration::from_millis(400 * (step as u64 + 1)));
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
    fn error_data_object_is_the_revert_payload() {
        let text = r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted","data":"0xabcd"}}"#;
        let parsed = parse_response(200, text).unwrap();
        assert_eq!(parsed.error_code, Some(3));
        assert_eq!(parsed.error_data.unwrap(), vec![0xab, 0xcd]);
    }
}
