//! Seeds in `corpus/mainnet.yaml`.
//!
//! The file names the contracts the chain does not point at: the wrapper, the
//! enumeration roots, and the quote contracts. A list root is probed. A log
//! root carries its event. Symbols and decimals are read from the token.

use alloy_primitives::{keccak256, Address, B256};
use std::str::FromStr;
use std::sync::OnceLock;

use crate::catalog::{kind_name, METRIC};

const TEXT: &str = include_str!("../../../corpus/mainnet.yaml");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wrap {
    pub token: Address,
    pub wrapper: Address,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Root {
    pub name: String,
    pub kind: u8,
    pub address: Address,
    pub quote: Option<Address>,
    pub topics: Vec<B256>,
    /// Probe this root for a list view. Log-only roots leave this false.
    pub list: bool,
    /// This address enumerates Curve registries. It is not itself a pool list.
    pub registries: bool,
}

impl Root {
    pub(crate) fn logs(&self) -> bool {
        !self.topics.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    pub wraps: Vec<Wrap>,
    pub multicall: Address,
    pub roots: Vec<Root>,
}

impl Document {
    pub(crate) fn by_name(&self, name: &str) -> &Root {
        self.roots
            .iter()
            .find(|root| root.name == name)
            .unwrap_or_else(|| panic!("seed {name}"))
    }

    pub(crate) fn by_address(&self, address: Address) -> Option<&Root> {
        self.roots.iter().find(|root| root.address == address)
    }

    pub(crate) fn kind_address(&self, kind: u8) -> Option<Address> {
        let mut found = self
            .roots
            .iter()
            .filter(|root| root.kind == kind && !root.registries);
        let first = found.next()?;
        if found.next().is_some() {
            return None;
        }
        Some(first.address)
    }
}

pub(crate) fn document() -> &'static Document {
    static DOC: OnceLock<Document> = OnceLock::new();
    DOC.get_or_init(|| parse_document(TEXT).expect("corpus/mainnet.yaml"))
}

/// The one enumeration root of `kind`. Several roots of one kind have no single home.
pub fn root_address(kind: u8) -> Option<Address> {
    document().kind_address(kind)
}

/// Quote contract for this factory, or the only quoted root of `kind`.
pub(crate) fn quoter(kind: u8, factory: Option<Address>) -> Option<Address> {
    let doc = document();
    if let Some(factory) = factory {
        if let Some(root) = doc.by_address(factory) {
            return root.quote;
        }
    }
    let mut quotes = doc
        .roots
        .iter()
        .filter(|root| root.kind == kind)
        .filter_map(|root| root.quote);
    let first = quotes.next()?;
    if quotes.next().is_some() {
        return None;
    }
    Some(first)
}

pub(crate) fn parse_document(text: &str) -> Result<Document, String> {
    let value = parse_yaml(text)?;
    let map = value.as_map().ok_or("document")?;
    let wraps = map_list(map, "wraps")?
        .iter()
        .map(|item| {
            let item = item.as_map().ok_or("wrap")?;
            Ok(Wrap {
                token: address(map_str(item, "token")?)?,
                wrapper: address(map_str(item, "wrapper")?)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let multicall = address(map_str(map, "multicall")?)?;
    let roots = map_list(map, "roots")?
        .iter()
        .map(root_of)
        .collect::<Result<Vec<_>, String>>()?;
    if wraps.is_empty() || roots.is_empty() {
        return Err("seeds".into());
    }
    Ok(Document {
        wraps,
        multicall,
        roots,
    })
}

fn root_of(value: &Value) -> Result<Root, String> {
    let map = value.as_map().ok_or("root")?;
    let name = map_str(map, "name")?.to_string();
    let kind = kind_of(map_str(map, "kind")?)?;
    let home = address(map_str(map, "address")?)?;
    let quote = match map_get(map, "quote") {
        Some(value) => Some(address(value.as_str().ok_or("quote")?)?),
        None => None,
    };
    let mut topics = Vec::new();
    if let Some(event) = map_get(map, "event") {
        topics.push(event_topic(event.as_str().ok_or("event")?)?);
    }
    if let Some(events) = map_get(map, "events") {
        for event in events.as_list().ok_or("events")? {
            topics.push(event_topic(event.as_str().ok_or("event")?)?);
        }
    }
    if let Some(topic) = map_get(map, "topic") {
        topics.push(topic_of(topic.as_str().ok_or("topic")?)?);
    }
    if let Some(extra) = map_get(map, "topics") {
        for topic in extra.as_list().ok_or("topics")? {
            topics.push(topic_of(topic.as_str().ok_or("topic")?)?);
        }
    }
    let registries = map_bool(map, "registries").unwrap_or(false);
    let list = map_bool(map, "list").unwrap_or(!registries && topics.is_empty());
    if registries && (list || !topics.is_empty()) {
        return Err(format!("{name} is a registry provider"));
    }
    Ok(Root {
        name,
        kind,
        address: home,
        quote,
        topics,
        list,
        registries,
    })
}

fn kind_of(name: &str) -> Result<u8, String> {
    for kind in 1..=METRIC {
        if kind_name(kind) == name {
            return Ok(kind);
        }
    }
    Err(format!("kind {name}"))
}

fn address(text: &str) -> Result<Address, String> {
    Address::from_str(text).map_err(|_| format!("address {text}"))
}

fn event_topic(signature: &str) -> Result<B256, String> {
    if signature.is_empty() || signature.contains(' ') {
        return Err(format!("event {signature}"));
    }
    Ok(B256::from(keccak256(signature.as_bytes())))
}

fn topic_of(text: &str) -> Result<B256, String> {
    let hex = text.strip_prefix("0x").unwrap_or(text);
    if hex.len() != 64 {
        return Err(format!("topic {text}"));
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| format!("topic {text}"))?;
    }
    Ok(B256::from(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    String(String),
    Bool(bool),
    Map(Vec<(String, Value)>),
    List(Vec<Value>),
}

impl Value {
    fn as_map(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Map(items) => Some(items),
            _ => None,
        }
    }

    fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }
}

fn map_get<'a>(map: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

fn map_str<'a>(map: &'a [(String, Value)], key: &str) -> Result<&'a str, String> {
    map_get(map, key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn map_list<'a>(map: &'a [(String, Value)], key: &str) -> Result<&'a [Value], String> {
    map_get(map, key)
        .and_then(Value::as_list)
        .ok_or_else(|| format!("missing {key}"))
}

fn map_bool(map: &[(String, Value)], key: &str) -> Option<bool> {
    match map_get(map, key) {
        Some(Value::Bool(value)) => Some(*value),
        _ => None,
    }
}

struct Line {
    indent: usize,
    text: String,
}

fn parse_yaml(text: &str) -> Result<Value, String> {
    let lines = yaml_lines(text);
    let mut at = 0;
    let value = parse_block(&lines, &mut at, 0)?;
    if at != lines.len() {
        return Err(format!("yaml at {}", lines[at].text));
    }
    Ok(value)
}

fn yaml_lines(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let code = strip_comment(raw);
        if code.trim().is_empty() {
            continue;
        }
        let indent = code.len() - code.trim_start().len();
        lines.push(Line {
            indent,
            text: code.trim().to_string(),
        });
    }
    lines
}

fn strip_comment(line: &str) -> &str {
    let mut quote = false;
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'"' {
            quote = !quote;
        } else if *byte == b'#' && !quote {
            return &line[..index];
        }
    }
    line
}

fn parse_block(lines: &[Line], at: &mut usize, indent: usize) -> Result<Value, String> {
    if *at >= lines.len() {
        return Err("yaml ended".into());
    }
    if lines[*at].text.starts_with("- ") {
        return parse_list(lines, at, indent);
    }
    parse_map(lines, at, indent)
}

fn parse_list(lines: &[Line], at: &mut usize, indent: usize) -> Result<Value, String> {
    let mut items = Vec::new();
    while *at < lines.len() && lines[*at].indent == indent && lines[*at].text.starts_with("- ") {
        let rest = lines[*at].text[2..].trim().to_string();
        *at += 1;
        if rest.is_empty() {
            items.push(parse_block(lines, at, indent + 2)?);
            continue;
        }
        if let Some((key, value)) = split_field(&rest) {
            let mut map = vec![(key, scalar(&value)?)];
            while *at < lines.len()
                && lines[*at].indent > indent
                && !lines[*at].text.starts_with("- ")
            {
                map.push(parse_field(lines, at)?);
            }
            items.push(Value::Map(map));
        } else {
            items.push(scalar(&rest)?);
        }
    }
    if items.is_empty() {
        return Err("empty list".into());
    }
    Ok(Value::List(items))
}

fn parse_map(lines: &[Line], at: &mut usize, indent: usize) -> Result<Value, String> {
    let mut map = Vec::new();
    while *at < lines.len() && lines[*at].indent == indent && !lines[*at].text.starts_with("- ") {
        map.push(parse_field(lines, at)?);
    }
    if map.is_empty() {
        return Err("empty map".into());
    }
    Ok(Value::Map(map))
}

fn parse_field(lines: &[Line], at: &mut usize) -> Result<(String, Value), String> {
    let indent = lines[*at].indent;
    let (key, value) =
        split_field(&lines[*at].text).ok_or_else(|| format!("field {}", lines[*at].text))?;
    *at += 1;
    if value.is_empty() {
        if *at < lines.len() && lines[*at].indent > indent {
            return Ok((key, parse_block(lines, at, lines[*at].indent)?));
        }
        return Err(format!("empty {key}"));
    }
    Ok((key, scalar(&value)?))
}

fn split_field(text: &str) -> Option<(String, String)> {
    let (key, value) = text.split_once(':')?;
    if key.is_empty() || key.contains(' ') {
        return None;
    }
    Some((key.to_string(), value.trim().to_string()))
}

fn scalar(text: &str) -> Result<Value, String> {
    if text == "true" {
        return Ok(Value::Bool(true));
    }
    if text == "false" {
        return Ok(Value::Bool(false));
    }
    if let Some(inner) = text
        .strip_prefix('"')
        .and_then(|body| body.strip_suffix('"'))
    {
        return Ok(Value::String(inner.to_string()));
    }
    if text.is_empty() {
        return Err("empty scalar".into());
    }
    Ok(Value::String(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{UNI_V2, UNI_V3, V2_FORK};
    use alloy_primitives::b256;

    #[test]
    fn the_seed_file_names_each_root_once() {
        let doc = document();
        assert!(doc.wraps.iter().any(|wrap| wrap.token == Address::ZERO));
        assert_ne!(doc.multicall, Address::ZERO);
        let mut names = std::collections::BTreeSet::new();
        for root in &doc.roots {
            assert!(names.insert(root.name.clone()), "duplicate {}", root.name);
            assert_ne!(root.address, Address::ZERO);
        }
        assert!(doc.roots.iter().any(|root| root.registries));
        assert!(doc.by_name("uni-v2").list);
        assert!(doc.by_name("uni-v3").logs());
        assert!(!doc.by_name("uni-v3").list);
        assert!(doc.by_name("metric").list && doc.by_name("metric").logs());
        assert_eq!(
            doc.kind_address(UNI_V2),
            Some(doc.by_name("uni-v2").address)
        );
        assert!(doc.kind_address(V2_FORK).is_none());
        assert_eq!(quoter(UNI_V3, None), doc.by_name("uni-v3").quote);
        let quiet = doc
            .roots
            .iter()
            .find(|root| root.kind == V2_FORK && root.quote.is_none())
            .unwrap();
        assert_eq!(quoter(V2_FORK, Some(quiet.address)), None);
    }

    #[test]
    fn log_topics_are_the_events_the_decoder_already_matches() {
        let doc = document();
        assert_eq!(
            doc.by_name("uni-v3").topics[0],
            b256!("783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118")
        );
        assert_eq!(
            doc.by_name("uni-v4").topics[0],
            B256::from(keccak256(
                b"Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)"
            ))
        );
        assert_eq!(
            doc.by_name("balancer-v2").topics[0],
            b256!("3c13bc30b8e878c53fd2a36b679409c073afd75950be43d8858768e956fbc20e")
        );
        assert_eq!(
            doc.by_name("balancer-v2").topics[1],
            b256!("f5847d3f2197b16cdcd2098ec95d0905cd1abdaf415f07bb7cef2bba8ac5dec4")
        );
        assert_eq!(
            doc.by_name("metric").topics[0],
            b256!("4b36a0ddce54edb36597ee7d496df06c53fe875aba9d7257534a38d5177899aa")
        );
    }
}
