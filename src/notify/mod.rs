//! Announcing finished batches.
//!
//! The scanner knows exactly when a folder's pipeline is done and which
//! directories it touched. It says so to whoever indexes those trees: Nextcloud
//! sees none of these writes and needs a rescan. media-organizer does not trigger
//! that rescan (the container has neither Docker nor `occ`); it only publishes the
//! fact, through two optional sinks:
//!
//! * the **event file** `<dir>/<folder>.json`, atomic and durable: a consumer that
//!   was down reads it when it comes back ([`file`]);
//! * a **webhook**, a `POST` of the same JSON, for anything that wants a push
//!   ([`webhook`]).
//!
//! **Debounce.** Uploads trickle in and are organized as they arrive, so announcing
//! every scan's batch would make the consumer rescan every few seconds. What
//! changed is instead *held* ([`Pending`]) and published as one event once the
//! input has been quiet for `quiet_secs`, or `max_wait_secs` after the first held
//! change, whichever comes first — the cap is what keeps a continuous flow from
//! postponing the announcement forever. Held changes are persisted, so a restart
//! does not forget them.
//!
//! This is the one write that is not part of the pipeline's phases: it goes
//! outside every input and library (validated in the config), after the commit,
//! and a failure is a warning, never a failed batch.

mod event;
pub mod file;
mod state;
mod webhook;

pub use event::{BatchEvent, Changes, Touched};
pub use state::{LastEvent, Pending, NOTIFY_FILE};

use crate::config::{FolderConfig, NotifyConfig};
use crate::error::NotifyError;
use crate::http::Url;
use chrono::{DateTime, Local, TimeZone};
use state::NotifyState;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// What announcing a batch came to.
#[derive(Debug, Clone)]
pub struct Announced {
    pub event: BatchEvent,
    /// The same batch as `/healthz` reports it.
    pub last: LastEvent,
    /// One message per sink that failed; empty when everything was delivered.
    pub errors: Vec<String>,
}

pub struct Notifier {
    dir: Option<PathBuf>,
    webhook: Option<Url>,
    quiet_secs: u64,
    max_wait_secs: u64,
    state: NotifyState,
}

impl Notifier {
    /// `None` when `[notify]` configures no sink: nothing is written, not even
    /// the counter.
    pub fn new(config: &NotifyConfig, state_dir: &Path) -> Option<Self> {
        if !config.enabled() {
            return None;
        }
        // Already validated with the config; a failure here would be a bug, but
        // it must not stop the sink that works.
        let webhook = config.webhook_url.as_deref().and_then(|raw| {
            Url::parse(raw)
                .map_err(|e| warn!(error = %e, "notify.webhook_url ignored"))
                .ok()
        });
        if let Some(dir) = &config.dir {
            if let Err(e) = file::check_writable(dir) {
                warn!(dir = %dir.display(), error = %e,
                      "notify.dir is not writable; batch events will not be written until it is");
            }
        }
        Some(Self {
            dir: config.dir.clone(),
            webhook,
            quiet_secs: config.quiet_secs,
            max_wait_secs: config.max_wait_secs,
            state: NotifyState::load(state_dir),
        })
    }

    /// The last event announced per folder label, as recorded on disk.
    pub fn last_events(&self) -> BTreeMap<String, LastEvent> {
        self.state.last_events()
    }

    /// Called after each folder's scan, with what that scan changed (possibly
    /// nothing: a quiet scan is what lets the quiet period elapse). Holds the
    /// changes, and publishes everything held once it is due. `now` is Unix seconds.
    ///
    /// `None` while nothing is waiting, or while the wait is not over; no `seq` is
    /// consumed then. Never fails: each sink's error is logged and returned in
    /// [`Announced::errors`], and the other sink is still tried.
    pub fn tick(&mut self, folder: &FolderConfig, changes: Changes, now: u64) -> Option<Announced> {
        self.run(folder, changes, now, false)
    }

    /// Like [`Notifier::tick`], but publishes at once whatever the debounce: for
    /// `recover`, which exits right after and has no later scan to flush.
    pub fn announce_now(
        &mut self,
        folder: &FolderConfig,
        changes: Changes,
        now: u64,
    ) -> Option<Announced> {
        self.run(folder, changes, now, true)
    }

    fn run(
        &mut self,
        folder: &FolderConfig,
        changes: Changes,
        now: u64,
        force: bool,
    ) -> Option<Announced> {
        let label = folder.label();
        let touched = changes.into_touched();
        if !touched.is_empty() {
            let first = self.state.pending(&label).is_none();
            if let Err(e) = self.state.hold(&label, touched, now) {
                warn!(folder = %label, error = %e, "cannot save the held announcement");
            }
            if first && !force && !self.due(&label, now) {
                info!(
                    folder = %label, quiet_secs = self.quiet_secs, max_wait_secs = self.max_wait_secs,
                    "batch announcement held until the input is quiet"
                );
            }
        }
        if self.state.pending(&label).is_none() || !(force || self.due(&label, now)) {
            return None;
        }
        self.publish(folder, now)
    }

    /// Whether what is held for `folder` should go out now.
    fn due(&self, folder: &str, now: u64) -> bool {
        let Some(held) = self.state.pending(folder) else {
            return false;
        };
        self.quiet_secs == 0
            // The clock went back: a wait cannot be measured, so do not hold on it.
            || now < held.last_at
            || now - held.last_at >= self.quiet_secs
            || now - held.first_at >= self.max_wait_secs
    }

    /// Publish everything held for `folder` as one event.
    fn publish(&mut self, folder: &FolderConfig, now: u64) -> Option<Announced> {
        let label = folder.label();
        let slug = folder.slug();
        let held = self.state.pending(&label)?.clone();

        let floor = self
            .dir
            .as_deref()
            .and_then(|dir| file::last_seq(dir, &slug))
            .unwrap_or(0);
        let seq = self.state.next_seq(&label, floor);
        let event = held.touched.into_event(&label, seq, local_time(now));

        // Recorded before publishing: a crash leaves a gap, never a reused number.
        let last = LastEvent {
            seq,
            finished_at: i64::try_from(now).unwrap_or(i64::MAX),
            committed: event.committed,
            deleted_sources: event.deleted_sources,
        };
        if let Err(e) = self.state.announced(&label, last.clone()) {
            warn!(folder = %label, error = %e, "cannot save the batch counter");
        }

        let json = event.to_json();
        let mut errors = Vec::new();
        if let Some(dir) = &self.dir {
            if let Err(source) = file::publish(dir, &slug, &json) {
                let error = NotifyError::File {
                    dir: dir.clone(),
                    source,
                };
                errors.push(failed(&label, "event file", error));
            }
        }
        if let Some(url) = &self.webhook {
            if let Err(error) = webhook::post(url, &json) {
                errors.push(failed(&label, "webhook", error));
            }
        }

        if errors.is_empty() {
            info!(
                folder = %label, seq, committed = event.committed,
                deleted_sources = event.deleted_sources, "batch announced"
            );
        }
        Some(Announced {
            event,
            last,
            errors,
        })
    }
}

fn failed(folder: &str, sink: &str, error: NotifyError) -> String {
    warn!(folder = %folder, sink, error = %error, "cannot announce the batch");
    error.to_string()
}

fn local_time(unix_secs: u64) -> DateTime<Local> {
    i64::try_from(unix_secs)
        .ok()
        .and_then(|secs| Local.timestamp_opt(secs, 0).single())
        .unwrap_or_else(Local::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const T0: u64 = 1_792_687_600;

    fn folder() -> FolderConfig {
        config::parse(
            "[[folders]]\nname = \"jordan\"\ninput = \"/in/jordan\"\nlibrary = \"/lib/jordan\"",
        )
        .unwrap()
        .folders
        .remove(0)
    }

    fn changes(files: usize) -> Changes {
        changes_in(files, "2026/10")
    }

    fn changes_in(files: usize, month: &str) -> Changes {
        let mut c = Changes::new(Path::new("/in/jordan"), Path::new("/lib/jordan"));
        for i in 0..files {
            c.committed(
                Path::new(&format!("/in/jordan/{i}.jpg")),
                Path::new(&format!("/lib/jordan/{month}/{i}.jpg")),
            );
        }
        c
    }

    /// A scan that organized nothing.
    fn quiet() -> Changes {
        changes(0)
    }

    fn config_for(dir: &Path, webhook: Option<&str>, quiet: u64, max_wait: u64) -> NotifyConfig {
        NotifyConfig {
            dir: Some(dir.to_path_buf()),
            webhook_url: webhook.map(String::from),
            quiet_secs: quiet,
            max_wait_secs: max_wait,
        }
    }

    /// No debounce: every batch goes out at once.
    fn notifier(dir: &Path, state: &Path, webhook: Option<&str>) -> Notifier {
        Notifier::new(&config_for(dir, webhook, 0, 0), state).unwrap()
    }

    fn debounced(dir: &Path, state: &Path, quiet: u64, max_wait: u64) -> Notifier {
        Notifier::new(&config_for(dir, None, quiet, max_wait), state).unwrap()
    }

    fn read_event(dir: &Path) -> BatchEvent {
        serde_json::from_slice(&std::fs::read(dir.join("jordan.json")).unwrap()).unwrap()
    }

    #[test]
    fn no_sink_means_no_notifier() {
        let state = tempfile::tempdir().unwrap();
        assert!(Notifier::new(&NotifyConfig::default(), state.path()).is_none());
        assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 0);
    }

    // -- without debounce -------------------------------------------------------

    #[test]
    fn a_batch_is_announced_once_with_its_counts() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = notifier(events.path(), state.path(), None);

        let announced = n.tick(&folder(), changes(12), T0).unwrap();

        assert!(announced.errors.is_empty());
        let event = read_event(events.path());
        assert_eq!(event, announced.event);
        assert_eq!(
            (event.folder.as_str(), event.seq, event.committed),
            ("jordan", 1, 12)
        );
        assert_eq!(event.library_dirs, ["2026/10"]);
    }

    #[test]
    fn an_empty_batch_is_not_announced_and_consumes_no_seq() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = notifier(events.path(), state.path(), None);

        assert!(n.tick(&folder(), quiet(), T0).is_none());
        assert!(!events.path().join("jordan.json").exists());
        assert_eq!(n.tick(&folder(), changes(1), T0).unwrap().event.seq, 1);
    }

    #[test]
    fn seq_grows_per_batch_and_the_file_holds_the_latest() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = notifier(events.path(), state.path(), None);
        for expected in 1..=3 {
            assert_eq!(
                n.tick(&folder(), changes(1), T0).unwrap().event.seq,
                expected
            );
        }
        assert_eq!(read_event(events.path()).seq, 3);
    }

    #[test]
    fn seq_continues_after_a_restart() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        notifier(events.path(), state.path(), None).tick(&folder(), changes(1), T0);

        let mut restarted = notifier(events.path(), state.path(), None);
        assert_eq!(restarted.last_events()["jordan"].seq, 1);
        assert_eq!(
            restarted.tick(&folder(), changes(1), T0).unwrap().event.seq,
            2
        );
    }

    #[test]
    fn seq_continues_from_the_event_file_when_the_counter_is_lost() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = notifier(events.path(), state.path(), None);
        n.tick(&folder(), changes(1), T0);
        n.tick(&folder(), changes(1), T0);

        std::fs::remove_file(state.path().join(NOTIFY_FILE)).unwrap();
        let mut lost = notifier(events.path(), state.path(), None);
        assert_eq!(lost.tick(&folder(), changes(1), T0).unwrap().event.seq, 3);
    }

    #[test]
    fn an_unwritable_dir_is_reported_not_fatal() {
        let (state, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let blocker = scratch.path().join("events");
        std::fs::write(&blocker, b"x").unwrap();
        let mut n = notifier(&blocker, state.path(), None);

        let announced = n.tick(&folder(), changes(2), T0).unwrap();

        assert_eq!(announced.errors.len(), 1);
        assert!(
            announced.errors[0].contains("event file"),
            "{:?}",
            announced.errors
        );
        // The number was still consumed: the consumer will see the gap.
        assert_eq!(n.tick(&folder(), changes(1), T0).unwrap().event.seq, 2);
    }

    /// A receiver answering `status`, returning the body it was sent.
    fn receiver(status: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/batch", listener.local_addr().unwrap());
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
            raw.split_once("\r\n\r\n").unwrap().1.to_string()
        });
        (url, handle)
    }

    #[test]
    fn the_webhook_receives_the_same_json_as_the_file() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (url, receiver) = receiver("200 OK");
        let mut n = notifier(events.path(), state.path(), Some(&url));

        let announced = n.tick(&folder(), changes(3), T0).unwrap();

        assert!(announced.errors.is_empty());
        let body = receiver.join().unwrap();
        assert_eq!(
            body.as_bytes(),
            std::fs::read(events.path().join("jordan.json")).unwrap()
        );
    }

    #[test]
    fn a_dead_webhook_does_not_stop_the_file() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut n = notifier(
            events.path(),
            state.path(),
            Some(&format!("http://127.0.0.1:{port}/batch")),
        );

        let announced = n.tick(&folder(), changes(1), T0).unwrap();

        assert_eq!(announced.errors.len(), 1);
        assert!(
            announced.errors[0].contains("webhook"),
            "{:?}",
            announced.errors
        );
        assert_eq!(read_event(events.path()).seq, 1);
    }

    #[test]
    fn a_failing_file_does_not_stop_the_webhook() {
        let (state, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let blocker = scratch.path().join("events");
        std::fs::write(&blocker, b"x").unwrap();
        let (url, receiver) = receiver("200 OK");
        let mut n = notifier(&blocker, state.path(), Some(&url));

        let announced = n.tick(&folder(), changes(1), T0).unwrap();

        assert_eq!(announced.errors.len(), 1);
        assert!(receiver.join().unwrap().contains("\"seq\":1"));
    }

    #[test]
    fn a_webhook_alone_works_without_a_dir() {
        let state = tempfile::tempdir().unwrap();
        let (url, receiver) = receiver("204 No Content");
        let cfg = NotifyConfig {
            dir: None,
            webhook_url: Some(url),
            quiet_secs: 0,
            max_wait_secs: 0,
        };
        let mut n = Notifier::new(&cfg, state.path()).unwrap();

        assert!(n.tick(&folder(), changes(1), T0).unwrap().errors.is_empty());
        receiver.join().unwrap();
        assert!(
            state.path().join(NOTIFY_FILE).exists(),
            "the counter still persists"
        );
    }

    // -- debounce: uploads trickle in, one event comes out -----------------------

    #[test]
    fn a_batch_is_held_until_the_input_has_been_quiet() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 600);

        assert!(n.tick(&folder(), changes(2), T0).is_none(), "held");
        assert!(
            n.tick(&folder(), quiet(), T0 + 30).is_none(),
            "still within the quiet period"
        );
        assert!(n.tick(&folder(), quiet(), T0 + 59).is_none());
        assert!(!events.path().join("jordan.json").exists());

        let announced = n.tick(&folder(), quiet(), T0 + 60).unwrap();
        assert_eq!((announced.event.seq, announced.event.committed), (1, 2));
    }

    #[test]
    fn trickling_uploads_become_one_event_with_everything_merged() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 600);

        // A file every 30 s: each scan has a little to announce, none goes out.
        assert!(n.tick(&folder(), changes_in(1, "2026/09"), T0).is_none());
        assert!(n
            .tick(&folder(), changes_in(2, "2026/10"), T0 + 30)
            .is_none());
        assert!(n
            .tick(&folder(), changes_in(1, "2026/10"), T0 + 60)
            .is_none());
        // The uploads stop. Quiet counts from the last change, not the first.
        assert!(n.tick(&folder(), quiet(), T0 + 100).is_none());
        let announced = n.tick(&folder(), quiet(), T0 + 120).unwrap();

        let e = announced.event;
        assert_eq!((e.seq, e.committed), (1, 4));
        assert_eq!(e.library_dirs, ["2026/09", "2026/10"]);
        // And that is all: the next scans announce nothing more.
        assert!(n.tick(&folder(), quiet(), T0 + 150).is_none());
        assert_eq!(read_event(events.path()).seq, 1);
    }

    #[test]
    fn a_continuous_flow_cannot_postpone_the_announcement_forever() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 300);

        // New files every 30 s, so the quiet period is never reached.
        let mut published = None;
        for step in 0..=10 {
            let now = T0 + step * 30;
            if let Some(a) = n.tick(&folder(), changes(1), now) {
                published = Some((now, a));
                break;
            }
        }

        let (now, announced) = published.expect("the cap must publish");
        assert_eq!(now, T0 + 300, "max_wait after the first held change");
        assert_eq!(announced.event.committed, 11);
        // The window restarts with the next change.
        assert!(n.tick(&folder(), changes(1), now + 30).is_none());
        let next = n.tick(&folder(), quiet(), now + 30 + 60).unwrap();
        assert_eq!((next.event.seq, next.event.committed), (2, 1));
    }

    #[test]
    fn no_seq_is_consumed_while_a_batch_is_held() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 600);
        n.tick(&folder(), changes(1), T0);
        n.tick(&folder(), changes(1), T0 + 10);
        assert!(n.last_events().is_empty());
        assert_eq!(n.tick(&folder(), quiet(), T0 + 70).unwrap().event.seq, 1);
    }

    #[test]
    fn held_changes_survive_a_restart_and_go_out_when_due() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        debounced(events.path(), state.path(), 60, 600).tick(&folder(), changes(3), T0);

        // The process died while holding; the next one finishes the job.
        let mut restarted = debounced(events.path(), state.path(), 60, 600);
        assert!(restarted.tick(&folder(), quiet(), T0 + 30).is_none());
        let announced = restarted.tick(&folder(), quiet(), T0 + 60).unwrap();
        assert_eq!((announced.event.seq, announced.event.committed), (1, 3));
    }

    #[test]
    fn a_clock_that_went_back_does_not_hold_forever() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 600);
        n.tick(&folder(), changes(1), T0);
        assert!(n.tick(&folder(), quiet(), T0 - 3_600).is_some());
    }

    #[test]
    fn announce_now_flushes_what_is_held_and_what_it_is_given() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 600);
        n.tick(&folder(), changes_in(2, "2026/09"), T0);

        let announced = n
            .announce_now(&folder(), changes_in(1, "2026/10"), T0 + 1)
            .unwrap();

        assert_eq!((announced.event.seq, announced.event.committed), (1, 3));
        assert_eq!(announced.event.library_dirs, ["2026/09", "2026/10"]);
    }

    #[test]
    fn announce_now_with_nothing_held_announces_nothing() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 600);
        assert!(n.announce_now(&folder(), quiet(), T0).is_none());
    }

    #[test]
    fn each_folder_is_debounced_on_its_own() {
        let (events, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut n = debounced(events.path(), state.path(), 60, 600);
        let other = config::parse(
            "[[folders]]\nname = \"sylvie\"\ninput = \"/in/sylvie\"\nlibrary = \"/lib/sylvie\"",
        )
        .unwrap()
        .folders
        .remove(0);
        let sylvie = || Changes::new(Path::new("/in/sylvie"), Path::new("/lib/sylvie"));

        n.tick(&folder(), changes(1), T0);
        // Sylvie's scan finds nothing: it must not reset jordan's quiet period.
        n.tick(&other, sylvie(), T0 + 50);

        assert!(n.tick(&folder(), quiet(), T0 + 60).is_some());
        assert!(n.tick(&other, sylvie(), T0 + 60).is_none());
    }
}
