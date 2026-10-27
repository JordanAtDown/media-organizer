//! The webhook: the same JSON as the event file, in a `POST`.
//!
//! One attempt, no retry: it runs inside the scan loop, so it is bounded by a
//! short timeout and a dead receiver costs a few seconds per batch at most. A
//! receiver that was down misses the event; the event file is the durable path.

use crate::error::NotifyError;
use crate::http::{self, Url};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

/// `POST` `json` to `url`; any 2xx counts as delivered.
pub fn post(url: &Url, json: &[u8]) -> Result<(), NotifyError> {
    let code = http::post_json(url, json, TIMEOUT).map_err(|source| NotifyError::Webhook {
        url: url.to_string(),
        source,
    })?;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(NotifyError::WebhookStatus {
            url: url.to_string(),
            code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A receiver answering `status`; returns its URL and what it was sent.
    fn receiver(status: &'static str) -> (Url, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/batch", listener.local_addr().unwrap())).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = String::new();
            let mut buf = [0u8; 2048];
            while !raw.contains("\r\n\r\n{") || !raw.ends_with('\n') {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0, "closed early: {raw}");
                raw.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").unwrap();
            raw
        });
        (url, handle)
    }

    #[test]
    fn a_2xx_is_delivery() {
        let (url, server) = receiver("204 No Content");
        post(&url, b"{\"seq\":3}\n").unwrap();
        assert!(server.join().unwrap().ends_with("{\"seq\":3}\n"));
    }

    #[test]
    fn a_5xx_is_a_failure_naming_the_code() {
        let (url, server) = receiver("500 Internal Server Error");
        let err = post(&url, b"{}\n").unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(err, NotifyError::WebhookStatus { code: 500, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_dead_receiver_is_a_failure_not_a_hang() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/batch")).unwrap();
        let err = post(&url, b"{}\n").unwrap_err();
        assert!(matches!(err, NotifyError::Webhook { .. }), "{err}");
    }
}
