//! Exporter client. The status process only reads `/status` and posts pause.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub(crate) fn status_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/status") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/status")
    }
}

pub(crate) fn fetch(url: &str) -> Result<Value, String> {
    let mut last = String::new();
    for attempt in 0..2 {
        match fetch_once(url) {
            Ok(value) => return Ok(value),
            Err(err) => {
                last = err;
                if attempt == 0 {
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
        }
    }
    Err(last)
}

pub(crate) fn post_control(status_url: &str, path: &str) -> Result<Value, String> {
    let (host, addr, _status_path) = endpoint(status_url)?;
    let mut stream = connect(&addr)?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .map_err(|err| err.to_string())?;
    let (code, body) = read_response(&mut stream)?;
    if code != 200 {
        return Err(format!("control {path} returned {code}"));
    }
    serde_json::from_str(&body).map_err(|err| format!("{err}"))
}

fn fetch_once(url: &str) -> Result<Value, String> {
    let (host, addr, path) = endpoint(url)?;
    let mut stream = connect(&addr)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|err| err.to_string())?;
    let (code, body) = read_response(&mut stream)?;
    if code != 200 {
        return Err(format!("status returned {code}"));
    }
    serde_json::from_str(&body).map_err(|err| format!("{err}"))
}

fn endpoint(url: &str) -> Result<(String, String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "status url needs http://".to_string())?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, "status"));
    let path = format!("/{path}");
    let addr = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    Ok((host.to_string(), addr, path))
}

fn connect(addr: &str) -> Result<TcpStream, String> {
    let stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|err| format!("{addr}: {err}"))?,
        Duration::from_millis(400),
    )
    .map_err(|err| err.to_string())?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(400)))
        .ok();
    Ok(stream)
}

fn read_response(stream: &mut TcpStream) -> Result<(u16, String), String> {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    loop {
        if let Some(response) = complete_response(&buf) {
            return Ok(response);
        }
        let n = stream.read(&mut tmp).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1_000_000 {
            return Err("status response too large".into());
        }
    }
    complete_response(&buf).ok_or_else(|| "truncated status response".into())
}

fn complete_response(buf: &[u8]) -> Option<(u16, String)> {
    let split = buf.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(buf.get(..split)?).ok()?;
    let code = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let length = head.lines().find_map(|line| {
        let value = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))?
            .1
            .trim();
        value.parse::<usize>().ok()
    })?;
    let start = split + 4;
    let body = buf.get(start..start + length)?;
    Some((code, String::from_utf8(body.to_vec()).ok()?))
}
