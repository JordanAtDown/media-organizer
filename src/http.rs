//! A minimal blocking HTTP/1.1 client, for the two things this service says over
//! HTTP: the container's health probe and the batch webhook.
//!
//! Plain `http://` only. The image carries no TLS stack and no `curl`, and both
//! uses are a local hop (loopback, or the Docker host), so a client library would
//! be a dependency for ~150 lines. Only the status code is read back; redirects,
//! proxies and response bodies are out of scope on purpose.
//!
//! **One deadline bounds the whole exchange** (name lookup, connection, sending,
//! reading the answer), not each step: the webhook runs inside the scan loop, and
//! a receiver that accepts the connection and then answers a byte at a time would
//! otherwise stall every scan far beyond the timeout.

use crate::error::HttpError;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// A status line is a few dozen bytes; anything longer is not HTTP.
const MAX_STATUS_LINE: usize = 1024;

/// The moment an exchange must be over.
struct Deadline(Instant);

impl Deadline {
    fn after(timeout: Duration) -> Self {
        Self(Instant::now() + timeout)
    }

    /// Time left, or a `TimedOut` error once it has passed. Never zero, because
    /// a zero timeout means "wait forever" to a socket.
    fn left(&self) -> io::Result<Duration> {
        let left = self.0.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(timed_out())
        } else {
            Ok(left)
        }
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "timed out")
}

/// A socket reports its own timeout as `WouldBlock` on Unix: say what it is.
fn clarify(e: io::Error) -> io::Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => timed_out(),
        _ => e,
    }
}

/// An `http://host[:port][/path]` address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    host: String,
    port: u16,
    /// Always starts with `/`; includes the query string.
    path: String,
}

impl Url {
    pub fn parse(raw: &str) -> Result<Self, HttpError> {
        let bad = |reason: &str| HttpError::Url {
            url: raw.to_string(),
            reason: reason.to_string(),
        };
        // A control character would let a value smuggle extra header lines.
        if raw.chars().any(|c| c.is_ascii_control() || c == ' ') {
            return Err(bad("must not contain spaces or control characters"));
        }
        let rest = match raw.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some((scheme, _)) if scheme.eq_ignore_ascii_case("https") => {
                return Err(bad(
                    "https is not supported (the image has no TLS stack); use http://",
                ))
            }
            _ => return Err(bad("must start with http://")),
        };
        let rest = rest.split('#').next().unwrap_or_default();
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        let path = match path {
            "" => "/".to_string(),
            p if p.starts_with('?') => format!("/{p}"),
            p => p.to_string(),
        };
        if authority.contains('@') {
            return Err(bad("credentials in the URL are not supported"));
        }
        let (host, port) = split_host_port(authority).ok_or_else(|| bad("bad host or port"))?;
        Ok(Self { host, port, path })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The `Host` header value: the port is left out when it is the default.
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            80 => host,
            port => format!("{host}:{port}"),
        }
    }

    /// The first address of the host. An IP literal needs no lookup; a name is
    /// resolved on a helper thread so a dead resolver cannot outlast the deadline
    /// (`getaddrinfo` has no timeout of its own). The thread is left to finish.
    fn resolve(&self, deadline: &Deadline) -> Result<SocketAddr, HttpError> {
        if let Ok(ip) = self.host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, self.port));
        }
        let failed = |why: &str| HttpError::Resolve(format!("{}:{} ({why})", self.host, self.port));

        let (tx, rx) = std::sync::mpsc::channel();
        let (host, port) = (self.host.clone(), self.port);
        std::thread::spawn(move || {
            let first = (host.as_str(), port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next());
            let _ = tx.send(first);
        });
        match rx.recv_timeout(deadline.left().map_err(|_| failed("timed out"))?) {
            Ok(Some(addr)) => Ok(addr),
            Ok(None) => Err(failed("no such host")),
            Err(_) => Err(failed("timed out")),
        }
    }
}

impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http://{}{}", self.host_header(), self.path)
    }
}

/// `host[:port]` or `[v6][:port]`, the host unbracketed. `None` when malformed.
fn split_host_port(authority: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner.split_once(']')?;
        let port = match after {
            "" => None,
            after => Some(after.strip_prefix(':')?),
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None => 80,
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
    };
    Some((host.to_string(), port))
}

/// `GET path` on an already-resolved address. Returns the status code.
pub fn get(addr: SocketAddr, host: &str, path: &str, timeout: Duration) -> Result<u16, HttpError> {
    send(
        addr,
        &request("GET", host, path, None),
        &Deadline::after(timeout),
    )
}

/// `POST` a JSON body to `url`. Returns the status code; the caller decides what
/// counts as success. `timeout` bounds the whole call, name lookup included.
pub fn post_json(url: &Url, body: &[u8], timeout: Duration) -> Result<u16, HttpError> {
    let deadline = Deadline::after(timeout);
    let request = request("POST", &url.host_header(), url.path(), Some(body));
    send(url.resolve(&deadline)?, &request, &deadline)
}

fn request(method: &str, host: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: media-organizer/{}\r\nConnection: close\r\n",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body.unwrap_or_default());
    out
}

fn send(addr: SocketAddr, request: &[u8], deadline: &Deadline) -> Result<u16, HttpError> {
    let mut stream = TcpStream::connect_timeout(&addr, deadline.left()?)
        .map_err(|source| HttpError::Connect { addr, source })?;

    // Socket timeouts apply per call, so they are re-armed with the time left
    // before each one: that is what makes the deadline global.
    let mut sent = 0;
    while sent < request.len() {
        stream.set_write_timeout(Some(deadline.left()?))?;
        match stream.write(&request[sent..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(n) => sent += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(clarify(e).into()),
        }
    }

    // Only the status line matters: do not wait for (or buffer) the body.
    let mut head = Vec::new();
    let mut buf = [0u8; 256];
    while !head.contains(&b'\n') && head.len() < MAX_STATUS_LINE {
        stream.set_read_timeout(Some(deadline.left()?))?;
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(clarify(e).into()),
        }
    }
    parse_status(
        String::from_utf8_lossy(&head)
            .lines()
            .next()
            .unwrap_or_default(),
    )
}

fn parse_status(line: &str) -> Result<u16, HttpError> {
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next().and_then(|c| c.parse().ok())) {
        (Some(version), Some(code)) if version.starts_with("HTTP/") => Ok(code),
        _ => Err(HttpError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    const TIMEOUT: Duration = Duration::from_secs(3);

    #[rstest]
    #[case("http://host", "host", 80, "/")]
    #[case("http://host:8123/batch", "host", 8123, "/batch")]
    #[case(
        "HTTP://host.docker.internal:9/a/b?x=1",
        "host.docker.internal",
        9,
        "/a/b?x=1"
    )]
    #[case("http://host?x=1", "host", 80, "/?x=1")]
    #[case("http://host:80/p#frag", "host", 80, "/p")]
    #[case("http://127.0.0.1:8080/", "127.0.0.1", 8080, "/")]
    #[case("http://[::1]:8080/p", "::1", 8080, "/p")]
    #[case("http://[::1]/p", "::1", 80, "/p")]
    fn urls_parse(#[case] raw: &str, #[case] host: &str, #[case] port: u16, #[case] path: &str) {
        let url = Url::parse(raw).unwrap();
        assert_eq!((url.host(), url.port(), url.path()), (host, port, path));
    }

    #[rstest]
    #[case("")]
    #[case("host:80/x")]
    #[case("ftp://host/x")]
    #[case("http://")]
    #[case("http://:80/x")]
    #[case("http://host:/x")]
    #[case("http://host:0/x")]
    #[case("http://host:99999/x")]
    #[case("http://host:abc/x")]
    #[case("http://user:pw@host/x")]
    #[case("http://host/a b")]
    #[case("http://host/a\r\nX-Evil: 1")]
    #[case("http://[::1/x")]
    #[case("http://[::1]x/x")]
    fn bad_urls_are_rejected(#[case] raw: &str) {
        assert!(
            matches!(Url::parse(raw), Err(HttpError::Url { .. })),
            "{raw}"
        );
    }

    #[test]
    fn https_is_refused_with_a_reason() {
        let err = Url::parse("https://host/x").unwrap_err().to_string();
        assert!(err.contains("https is not supported"), "{err}");
    }

    #[test]
    fn host_header_omits_only_the_default_port() {
        let host = |raw| Url::parse(raw).unwrap().host_header();
        assert_eq!(host("http://h/x"), "h");
        assert_eq!(host("http://h:8123/x"), "h:8123");
        assert_eq!(host("http://[::1]:8123/x"), "[::1]:8123");
    }

    #[test]
    fn status_lines_parse() {
        assert_eq!(parse_status("HTTP/1.1 204 No Content\r\n").unwrap(), 204);
        assert_eq!(parse_status("HTTP/1.0 503\r\n").unwrap(), 503);
        for bad in ["", "garbage", "HTTP/1.1 abc", "200 OK"] {
            assert!(
                matches!(parse_status(bad), Err(HttpError::Malformed)),
                "{bad}"
            );
        }
    }

    /// Answer one request with `status`, and hand back the raw request received.
    fn serve_once(status: &'static str) -> (SocketAddr, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 1024];
            // Read the head, then exactly Content-Length bytes of body.
            let (head_end, length) = loop {
                let n = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    break (end + 4, length);
                }
                assert!(n > 0, "connection closed before the head was complete");
            };
            while raw.len() < head_end + length {
                let n = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            String::from_utf8(raw).unwrap()
        });
        (addr, handle)
    }

    #[test]
    fn post_sends_a_json_body_and_returns_the_status() {
        let (addr, server) = serve_once("204 No Content");
        let url = Url::parse(&format!("http://{addr}/batch?x=1")).unwrap();

        let code = post_json(&url, br#"{"seq":1}"#, TIMEOUT).unwrap();

        assert_eq!(code, 204);
        let raw = server.join().unwrap();
        assert!(raw.starts_with("POST /batch?x=1 HTTP/1.1\r\n"), "{raw}");
        assert!(raw.contains(&format!("Host: {addr}\r\n")), "{raw}");
        assert!(raw.contains("Content-Type: application/json\r\n"), "{raw}");
        assert!(raw.contains("Content-Length: 9\r\n"), "{raw}");
        assert!(raw.ends_with("\r\n\r\n{\"seq\":1}"), "{raw}");
    }

    #[test]
    fn get_returns_the_status_whatever_it_is() {
        let (addr, server) = serve_once("503 Service Unavailable");
        assert_eq!(get(addr, "localhost", "/healthz", TIMEOUT).unwrap(), 503);
        let raw = server.join().unwrap();
        assert!(
            raw.starts_with("GET /healthz HTTP/1.1\r\nHost: localhost\r\n"),
            "{raw}"
        );
    }

    #[test]
    fn a_refused_connection_names_the_address() {
        // A port that was just released: the connection is refused at once.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let err = post_json(&url, b"{}", TIMEOUT).unwrap_err();
        assert!(matches!(err, HttpError::Connect { .. }), "{err}");
        assert!(err.to_string().contains(&port.to_string()), "{err}");
    }

    #[test]
    fn a_receiver_answering_a_byte_at_a_time_cannot_outlast_the_deadline() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let drip = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                // Never a full line, and each byte well inside the per-read timeout.
                while !stop.load(Ordering::SeqCst) {
                    if stream.write_all(b"H").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            })
        };

        let start = Instant::now();
        let err = post_json(&url, b"{}", Duration::from_millis(400)).unwrap_err();
        let took = start.elapsed();
        stop.store(true, Ordering::SeqCst);
        drip.join().unwrap();

        assert!(matches!(err, HttpError::Io(_)), "{err}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(took < Duration::from_secs(2), "took {took:?}");
    }

    #[test]
    fn the_deadline_covers_the_whole_call_not_each_step() {
        let deadline = Deadline::after(Duration::from_millis(60));
        assert!(deadline.left().is_ok());
        std::thread::sleep(Duration::from_millis(80));
        let err = deadline.left().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn an_ip_literal_needs_no_lookup_and_a_name_is_resolved() {
        let deadline = Deadline::after(Duration::from_secs(5));
        let ip = Url::parse("http://127.0.0.1:9/").unwrap();
        assert_eq!(
            ip.resolve(&deadline).unwrap(),
            "127.0.0.1:9".parse().unwrap()
        );
        let v6 = Url::parse("http://[::1]:9/").unwrap();
        assert_eq!(v6.resolve(&deadline).unwrap(), "[::1]:9".parse().unwrap());
        let name = Url::parse("http://localhost:9/").unwrap();
        assert_eq!(name.resolve(&deadline).unwrap().port(), 9);
    }

    #[test]
    fn a_silent_server_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let start = std::time::Instant::now();
        let err = post_json(&url, b"{}", Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, HttpError::Io(_)), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
