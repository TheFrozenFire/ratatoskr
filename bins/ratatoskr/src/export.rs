//! Local HTTP exporter. `GET /status` is JSON. `GET /metrics` is Prometheus text.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

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
        let _ = stream.set_nodelay(true);
        let _ = answer(&mut stream);
        finish(&mut stream);
    }
}

fn answer(stream: &mut TcpStream) -> io::Result<()> {
    let request = read_headers(stream)?;
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
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// Read until the header terminator. One `read` can return a prefix, and
/// closing with the rest unread makes the kernel reset the client.
fn read_headers(stream: &mut TcpStream) -> io::Result<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = Vec::with_capacity(256);
    let mut tmp = [0_u8; 512];
    loop {
        if buf.windows(4).any(|window| window == b"\r\n\r\n") || buf.len() > 8192 {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request headers"));
        }
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock
                    || err.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn finish(stream: &mut TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
    let mut junk = [0_u8; 256];
    loop {
        match stream.read(&mut junk) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}
