//! Local HTTP exporter. `GET /status` is JSON. `GET /metrics` is Prometheus text.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

pub fn serve(addr: &str) -> eyre::Result<()> {
    let addr: SocketAddr = addr
        .parse()
        .map_err(|err| eyre::eyre!("metrics address {addr}: {err}"))?;
    let listener = TcpListener::bind(addr).map_err(|err| eyre::eyre!("metrics {addr}: {err}"))?;
    std::thread::Builder::new()
        .name("metrics".into())
        .spawn(move || listen(listener))
        .map_err(|err| eyre::eyre!("metrics thread: {err}"))?;
    tracing::info!(%addr, "metrics exporter");
    Ok(())
}

fn listen(listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let _ = answer(&mut stream);
    }
}

fn answer(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buf = [0_u8; 2048];
    let n = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");
    let (content_type, body) = if path == "/metrics" {
        ("text/plain; version=0.0.4", venues::prometheus_text())
    } else {
        ("application/json", venues::status_json())
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}
