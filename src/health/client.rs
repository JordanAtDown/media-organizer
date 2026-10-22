//! Probe used by the container `HEALTHCHECK`, so the image needs no `curl`.

use crate::error::HealthError;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);

/// `GET /healthz` on the local endpoint. `Ok(())` for any 2xx.
///
/// `bind` is the server's bind address; a wildcard host (`0.0.0.0`, `[::]`) is
/// probed through loopback.
pub fn probe(bind: &str) -> Result<(), HealthError> {
    let err = |m: String| HealthError::Probe(m);
    let addr = loopback_for(bind)?;

    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)
        .map_err(|e| err(format!("cannot connect to {addr}: {e}")))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|e| err(e.to_string()))?;

    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| err(format!("no response: {e}")))?;

    let code: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| err("malformed HTTP response".into()))?;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(err(format!("healthz returned HTTP {code}")))
    }
}

fn loopback_for(bind: &str) -> Result<SocketAddr, HealthError> {
    let addr = bind
        .to_socket_addrs()
        .map_err(|e| HealthError::Probe(format!("bad bind address {bind:?}: {e}")))?
        .next()
        .ok_or_else(|| HealthError::Probe(format!("bad bind address {bind:?}")))?;
    Ok(if addr.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], addr.port()))
    } else {
        addr
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HealthConfig;
    use crate::health::{HealthServer, HealthState};
    use crate::scanner::{FolderSummary, ScanSummary};
    use crate::shutdown::Shutdown;
    use std::sync::Arc;

    #[test]
    fn wildcard_binds_are_probed_through_loopback() {
        assert_eq!(
            loopback_for("0.0.0.0:8080").unwrap().to_string(),
            "127.0.0.1:8080"
        );
        assert_eq!(
            loopback_for("127.0.0.1:9").unwrap().to_string(),
            "127.0.0.1:9"
        );
    }

    #[test]
    fn garbage_bind_is_an_error() {
        assert!(probe("missing-port").is_err());
    }

    #[test]
    fn nothing_listening_is_an_error() {
        // Port 1 is privileged and unused.
        assert!(matches!(probe("127.0.0.1:1"), Err(HealthError::Probe(_))));
    }

    #[test]
    fn probe_succeeds_against_a_healthy_server_and_fails_when_stale() {
        let cfg = HealthConfig {
            startup_grace_secs: 0,
            ..HealthConfig::default()
        };
        let state = Arc::new(HealthState::new(cfg, 30));
        let shutdown = Shutdown::new();
        let server = HealthServer::start("127.0.0.1:0", state.clone(), shutdown.clone()).unwrap();

        // No scan, no grace, no heartbeat: stale → 503 → probe fails.
        let stale = probe(server.addr()).unwrap_err().to_string();
        assert!(stale.contains("503"), "{stale}");

        state.record_scan(
            &ScanSummary {
                folders: vec![FolderSummary {
                    name: "p".into(),
                    input_exists: true,
                    ..Default::default()
                }],
            },
            Default::default(),
        );
        assert!(probe(server.addr()).is_ok());

        shutdown.request();
        server.join();
    }
}
