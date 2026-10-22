//! `/healthz` and `/livez` against a real scanner and a real socket.

mod common;

use common::*;
use media_organizer::config::HealthConfig;
use media_organizer::daemon;
use media_organizer::health::state::{evaluate, Inputs};
use media_organizer::health::{client, HealthServer, HealthState, Status};
use media_organizer::shutdown::Shutdown;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

fn get(addr: &str, path: &str) -> (u16, Value) {
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
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn serve(cfg: HealthConfig) -> (HealthServer, Arc<HealthState>, Shutdown) {
    let state = Arc::new(HealthState::new(cfg, 30));
    let shutdown = Shutdown::new();
    let server = HealthServer::start("127.0.0.1:0", state.clone(), shutdown.clone()).unwrap();
    (server, state, shutdown)
}

#[test]
fn a_real_scan_is_reflected_in_the_payload() {
    let env = Env::new();
    env.put("IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    env.put("holiday.jpg", &jpeg_without_exif()); // dated by mtime
    env.put("twin/IMG-20250626-WA0001.jpg", &jpeg_without_exif());
    let mut scanner = env.scanner("", "");
    let (server, state, shutdown) = serve(HealthConfig::default());

    daemon::run(
        &mut scanner,
        &state,
        &shutdown,
        Duration::from_secs(60),
        true,
    );
    let (code, body) = get(server.addr(), "/healthz");

    assert_eq!(code, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["scans_completed"], 1);
    assert_eq!(body["totals"]["organized"], 2);
    assert_eq!(body["totals"]["duplicates"], 1);
    assert_eq!(body["totals"]["by_date_source"]["filename"], 2);
    assert_eq!(body["totals"]["by_date_source"]["mtime"], 1);
    assert_eq!(body["pending_in_staging"], 0);
    assert_eq!(body["folders"][0]["input_exists"], true);
    assert_eq!(body["caches"]["duplicates"], 1);
    shutdown.request();
    server.join();
}

#[test]
fn a_missing_input_makes_the_service_degraded_not_dead() {
    let env = Env::new();
    let mut cfg = env.config("", "");
    cfg.folders[0].input = "/nonexistent/not-mounted-yet".into();
    let mut scanner = env.scanner_with(cfg, false);
    let (server, state, shutdown) = serve(HealthConfig::default());

    daemon::run(
        &mut scanner,
        &state,
        &shutdown,
        Duration::from_secs(60),
        true,
    );
    let (code, body) = get(server.addr(), "/healthz");

    assert_eq!((code, body["status"].as_str()), (200, Some("degraded")));
    assert_eq!(body["folders"][0]["input_exists"], false);
    shutdown.request();
    server.join();
}

#[test]
fn a_wedged_process_fails_healthz_while_livez_still_answers() {
    // No scan ever completes and no heartbeat beats, past the grace period:
    // the process is up (livez) but doing nothing (healthz 503).
    let (server, _state, shutdown) = serve(HealthConfig {
        startup_grace_secs: 0,
        ..Default::default()
    });

    let (live, _) = get(server.addr(), "/livez");
    let (health, body) = get(server.addr(), "/healthz");

    assert_eq!(live, 200);
    assert_eq!((health, body["status"].as_str()), (503, Some("stale")));
    assert!(
        client::probe(server.addr()).is_err(),
        "the container healthcheck fails too"
    );
    shutdown.request();
    server.join();
}

#[test]
fn the_probe_passes_once_a_scan_has_completed() {
    let env = Env::new();
    let mut scanner = env.scanner("", "");
    let (server, state, shutdown) = serve(HealthConfig {
        startup_grace_secs: 0,
        ..Default::default()
    });

    daemon::run(
        &mut scanner,
        &state,
        &shutdown,
        Duration::from_secs(60),
        true,
    );

    assert!(client::probe(server.addr()).is_ok());
    shutdown.request();
    server.join();
}

fn base() -> Inputs {
    Inputs {
        now: 100_000,
        uptime_secs: 50_000,
        scans_completed: 10,
        last_scan_end: 99_990,
        last_progress: 99_990,
        poll_interval_secs: 30,
        startup_grace_secs: 120,
        stale_intervals: 3,
        degraded: false,
    }
}

#[test]
fn a_long_scan_that_keeps_beating_stays_green_while_a_frozen_one_goes_red() {
    // The last scan *finished* an hour ago: a rule on that alone would call a
    // legitimately long scan stale.
    let running = |last_progress| Inputs {
        last_scan_end: 96_400,
        last_progress,
        ..base()
    };

    assert_eq!(
        evaluate(running(99_995)),
        Status::Ok,
        "still making progress"
    );
    assert_eq!(
        evaluate(running(99_000)),
        Status::Stale,
        "heartbeat frozen: a dead NFS stat()"
    );
}

#[test]
fn the_staleness_window_has_a_sixty_second_floor() {
    let quiet_for = |secs: u64, poll: u64| Inputs {
        last_scan_end: 100_000 - secs,
        last_progress: 100_000 - secs,
        poll_interval_secs: poll,
        stale_intervals: 1,
        ..base()
    };
    assert_eq!(
        evaluate(quiet_for(50, 5)),
        Status::Ok,
        "poll 5s, but never stricter than 60s"
    );
    assert_eq!(evaluate(quiet_for(70, 5)), Status::Stale);
    assert_eq!(
        evaluate(quiet_for(70, 600)),
        Status::Ok,
        "a long poll interval widens the window"
    );
}
