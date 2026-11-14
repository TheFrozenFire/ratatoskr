//! One row of the Ethereum mainnet feed directory.

use alloy_primitives::Address;
use serde_json::Value;
use std::collections::HashSet;
use std::str::FromStr;
pub use store_model::DirectoryRow;

/// `rddUrl` for Ethereum mainnet.
pub const DIRECTORY_URL: &str = "https://reference-data-directory.vercel.app/feeds-mainnet.json";

#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error("the directory is not a list")]
    NotAList,
    #[error("feed {index} has no proxy")]
    MissingProxy { index: usize },
    #[error("feed {index} proxy is not an address")]
    Proxy { index: usize },
    #[error("proxy {proxy} is repeated")]
    Repeated { proxy: Address },
    #[error("feed {proxy} {field} {reason}")]
    Field {
        proxy: Address,
        field: &'static str,
        reason: &'static str,
    },
}

pub fn parse_directory(body: &str) -> Result<Vec<DirectoryRow>, DirectoryError> {
    let value: Value = serde_json::from_str(body).map_err(|_| DirectoryError::NotAList)?;
    let Value::Array(rows) = value else {
        return Err(DirectoryError::NotAList);
    };
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let parsed = parse_row(index, row)?;
        if !seen.insert(parsed.proxy) {
            return Err(DirectoryError::Repeated {
                proxy: parsed.proxy,
            });
        }
        out.push(parsed);
    }
    Ok(out)
}

fn parse_row(index: usize, row: &Value) -> Result<DirectoryRow, DirectoryError> {
    let Value::Object(map) = row else {
        return Err(DirectoryError::MissingProxy { index });
    };
    let proxy = match map.get("proxyAddress") {
        None | Some(Value::Null) => return Err(DirectoryError::MissingProxy { index }),
        Some(Value::String(text)) => {
            Address::from_str(text).map_err(|_| DirectoryError::Proxy { index })?
        }
        Some(_) => return Err(DirectoryError::Proxy { index }),
    };
    let field = |name: &'static str, reason: &'static str| DirectoryError::Field {
        proxy,
        field: name,
        reason,
    };
    let string = |name: &'static str| -> Result<String, DirectoryError> {
        match map.get(name) {
            Some(Value::String(text)) => Ok(text.clone()),
            Some(_) => Err(field(name, "is not text")),
            None => Err(field(name, "is missing")),
        }
    };
    let whole = |name: &'static str| -> Result<u64, DirectoryError> {
        match map.get(name) {
            Some(Value::Number(number)) => number
                .as_u64()
                .ok_or_else(|| field(name, "is not a whole number")),
            _ => Err(field(name, "is not a number")),
        }
    };
    let threshold = match map.get("threshold") {
        Some(Value::Number(number)) => number.to_string(),
        _ => return Err(field("threshold", "is not a number")),
    };
    let decimals =
        u8::try_from(whole("decimals")?).map_err(|_| field("decimals", "does not fit"))?;
    let docs = match map.get("docs") {
        None | Some(Value::Null) => None,
        Some(Value::Object(docs)) => Some(docs),
        Some(_) => return Err(field("docs", "is not an object")),
    };
    let doc = |name: &'static str| -> Result<String, DirectoryError> {
        let Some(docs) = docs else {
            return Ok(String::new());
        };
        match docs.get(name) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(text)) => Ok(text.clone()),
            Some(_) => Err(field(name, "is not text")),
        }
    };
    let hidden = match docs.and_then(|docs| docs.get("hidden")) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(field("hidden", "is not a boolean")),
    };
    let secondary = match map.get("secondaryProxyAddress") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(
            Address::from_str(text)
                .map_err(|_| field("secondaryProxyAddress", "is not an address"))?,
        ),
        Some(_) => return Err(field("secondaryProxyAddress", "is not an address")),
    };
    let svr_label = match map.get("svrDisplayLabel") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(_) => return Err(field("svrDisplayLabel", "is not text")),
    };
    Ok(DirectoryRow {
        proxy,
        name: string("name")?,
        path: string("path")?,
        category: string("feedCategory")?,
        feed_type: string("feedType")?,
        contract_version: whole("contractVersion")?,
        decimals,
        multiply: string("multiply")?,
        heartbeat: whole("heartbeat")?,
        threshold,
        base_asset: doc("baseAsset")?,
        quote_asset: doc("quoteAsset")?,
        product_type: doc("productType")?,
        product_type_code: doc("productTypeCode")?,
        attribute_type: doc("attributeType")?,
        market_hours: doc("marketHours")?,
        shutdown_date: doc("shutdownDate")?,
        clic_product_name: doc("clicProductName")?,
        hidden,
        secondary_proxy: secondary,
        svr_label,
        directory_aggregator: Address::from_str(&string("contractAddress")?)
            .map_err(|_| field("contractAddress", "is not an address"))?,
    })
}
