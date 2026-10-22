use super::state::HealthState;
use crate::error::HealthError;
use crate::shutdown::Shutdown;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

/// How often the accept loop wakes to check for shutdown.
const POLL: Duration = Duration::from_millis(500);

/// A running `/healthz` + `/livez` endpoint.
pub struct HealthServer {
    handle: Option<JoinHandle<()>>,
    addr: String,
}

impl HealthServer {
    /// Bind **before** the scan loop starts: a port already taken must fail the
    /// startup loudly, not leave the daemon running unsupervised.
    pub fn start(
        bind: &str,
        state: Arc<HealthState>,
        shutdown: Shutdown,
    ) -> Result<Self, HealthError> {
        let server = Server::http(bind).map_err(|e| HealthError::Bind {
            addr: bind.to_string(),
            message: e.to_string(),
        })?;
        let addr = server
            .server_addr()
            .to_ip()
            .map_or_else(|| bind.to_string(), |a| a.to_string());

        let handle = std::thread::Builder::new()
            .name("mo-health".into())
            .spawn(move || serve(server, &state, &shutdown))
            .map_err(|e| HealthError::Bind {
                addr: bind.to_string(),
                message: e.to_string(),
            })?;
        tracing::info!(%addr, "health endpoint listening");
        Ok(Self {
            handle: Some(handle),
            addr,
        })
    }

    /// The address actually bound (useful when the port was 0).
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Wait for the server thread, after shutdown was requested.
    pub fn join(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(server: Server, state: &HealthState, shutdown: &Shutdown) {
    while !shutdown.is_requested() {
        match server.recv_timeout(POLL) {
            Ok(Some(req)) => handle(req, state),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "health server error");
                return;
            }
        }
    }
}

fn json_header() -> Header {
    Header::from_bytes("Content-Type", "application/json").expect("static header")
}

fn handle(req: Request, state: &HealthState) {
    if req.method() != &Method::Get {
        let _ = req.respond(Response::empty(405));
        return;
    }
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let result = match path.as_str() {
        // Alive as long as this thread answers: distinguishes "process dead"
        // from "process up but wedged" (which only /healthz reports).
        "/livez" => req.respond(Response::from_string("ok")),
        "/healthz" => {
            let snap = state.snapshot();
            let code = snap.status.http_code();
            let body = serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "{}".into());
            req.respond(
                Response::from_string(body)
                    .with_status_code(code)
                    .with_header(json_header()),
            )
        }
        _ => req.respond(Response::empty(404)),
    };
    if let Err(e) = result {
        tracing::debug!(error = %e, "health response failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HealthConfig;
    use crate::scanner::{FolderSummary, ScanSummary};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn get(addr: &str, path: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let code = raw.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (code, body)
    }

    fn start() -> (HealthServer, Arc<HealthState>, Shutdown) {
        let state = Arc::new(HealthState::new(HealthConfig::default(), 30));
        let shutdown = Shutdown::new();
        let server = HealthServer::start("127.0.0.1:0", state.clone(), shutdown.clone()).unwrap();
        (server, state, shutdown)
    }

    #[test]
    fn livez_always_answers() {
        let (server, _state, shutdown) = start();
        assert_eq!(get(server.addr(), "/livez"), (200, "ok".into()));
        shutdown.request();
        server.join();
    }

    #[test]
    fn healthz_reports_starting_with_200() {
        let (server, _state, shutdown) = start();
        let (code, body) = get(server.addr(), "/healthz");
        assert_eq!(code, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"],
            "starting"
        );
        shutdown.request();
        server.join();
    }

    #[test]
    fn healthz_turns_ok_after_a_scan() {
        let (server, state, shutdown) = start();
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
        let (code, body) = get(server.addr(), "/healthz");
        assert_eq!(code, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"],
            "ok"
        );
        shutdown.request();
        server.join();
    }

    #[test]
    fn unknown_path_is_404_and_post_is_405() {
        let (server, _state, shutdown) = start();
        assert_eq!(get(server.addr(), "/nope").0, 404);
        let mut s = TcpStream::connect(server.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "POST /healthz HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        assert!(raw.starts_with("HTTP/1.1 405"), "{raw}");
        shutdown.request();
        server.join();
    }

    #[test]
    fn a_taken_port_fails_at_startup() {
        let (server, state, shutdown) = start();
        let err = HealthServer::start(server.addr(), state, Shutdown::new());
        assert!(matches!(err, Err(HealthError::Bind { .. })));
        shutdown.request();
        server.join();
    }

    #[test]
    fn shutdown_stops_the_server_thread() {
        let (server, _state, shutdown) = start();
        shutdown.request();
        server.join(); // must return, not hang
    }
}
