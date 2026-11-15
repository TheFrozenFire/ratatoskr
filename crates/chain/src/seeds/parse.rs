use super::document::{Document, Root, Wrap};
use alloy_primitives::{keccak256, Address, B256};
use std::str::FromStr;

use store_model::{kind_name, METRIC};

pub(super) fn parse_document(text: &str) -> Result<Document, String> {
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
