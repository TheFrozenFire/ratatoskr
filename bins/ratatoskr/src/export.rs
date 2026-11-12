//! Local HTTP exporter. `GET /status` is JSON. `GET /metrics` is Prometheus text.
//! `POST /pause` and `POST /resume` stop and start stage claims. Peer sessions
//! stay connected.

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
    let method = request.split_whitespace().next().unwrap_or("GET");
    let (content_type, body) = respond(method, path);
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

fn respond(method: &str, path: &str) -> (&'static str, String) {
    match (method, path) {
        ("POST", "/pause") => {
            venues::pause_work();
            ("application/json", venues::status_json())
        }
        ("POST", "/resume") => {
            venues::resume_work();
            ("application/json", venues::status_json())
        }
        (_, "/metrics") => ("text/plain; version=0.0.4", venues::prometheus_text()),
        _ => ("application/json", venues::status_json()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    struct ResumeOnDrop;
    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            venues::resume_work();
        }
    }

    fn exchange(request: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            answer(&mut stream).unwrap();
            finish(&mut stream);
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).unwrap();
        server.join().unwrap();
        buf
    }

    #[test]
    fn post_pause_and_resume_round_trip() {
        let _one = one_at_a_time();
        let _guard = ResumeOnDrop;
        venues::resume_work();
        let paused = exchange(
            "POST /pause HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(paused.starts_with("HTTP/1.1 200 "));
        assert!(paused.contains("\"paused\":true"));
        assert!(venues::work_paused());
        let resumed = exchange(
            "POST /resume HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(resumed.contains("\"paused\":false"));
        assert!(!venues::work_paused());
    }

    #[test]
    fn get_status_leaves_work_running() {
        let _one = one_at_a_time();
        let _guard = ResumeOnDrop;
        venues::resume_work();
        let body = exchange("GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert!(body.contains("\"paused\":false"));
        assert!(!venues::work_paused());
    }
}
