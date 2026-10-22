//! Graceful shutdown: SIGTERM/SIGINT set a flag **and** wake the poll sleep, so
//! `docker stop` is answered immediately instead of after the next one-second tick.

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct Shutdown {
    inner: Arc<Inner>,
}

struct Inner {
    flag: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

impl Shutdown {
    /// A shutdown handle not wired to any signal; call [`request`](Self::request)
    /// yourself. Used by tests and by one-shot runs.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                flag: AtomicBool::new(false),
                lock: Mutex::new(()),
                wake: Condvar::new(),
            }),
        }
    }

    /// Wire SIGTERM and SIGINT to this handle.
    pub fn install() -> io::Result<Self> {
        Self::install_for(&[SIGTERM, SIGINT])
    }

    pub fn install_for(signals: &[i32]) -> io::Result<Self> {
        let shutdown = Self::new();
        let mut signals = Signals::new(signals)?;
        let handle = shutdown.clone();
        std::thread::Builder::new()
            .name("mo-signals".into())
            .spawn(move || {
                if signals.forever().next().is_some() {
                    tracing::info!("shutdown signal received, stopping");
                    handle.request();
                }
            })?;
        Ok(shutdown)
    }

    /// Ask everything to stop and wake any waiter.
    pub fn request(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        let _guard = self.inner.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.wake.notify_all();
    }

    pub fn is_requested(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// The raw flag, for code that only polls (the scanner).
    pub fn flag(&self) -> &AtomicBool {
        &self.inner.flag
    }

    /// Sleep up to `timeout`; returns `true` as soon as shutdown is requested.
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = self.inner.lock.lock().unwrap_or_else(|e| e.into_inner());
        let (_guard, _) = self
            .inner
            .wake
            .wait_timeout_while(guard, timeout, |_| !self.is_requested())
            .unwrap_or_else(|e| e.into_inner());
        self.is_requested()
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn wait_times_out_when_nothing_happens() {
        let s = Shutdown::new();
        let start = Instant::now();
        assert!(!s.wait(Duration::from_millis(50)));
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn request_wakes_a_long_wait_immediately() {
        let s = Shutdown::new();
        let waiter = s.clone();
        let start = Instant::now();
        let t = std::thread::spawn(move || waiter.wait(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(50));
        s.request();
        assert!(t.join().unwrap());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn already_requested_returns_without_waiting() {
        let s = Shutdown::new();
        s.request();
        let start = Instant::now();
        assert!(s.wait(Duration::from_secs(30)));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(s.is_requested() && s.flag().load(Ordering::SeqCst));
    }

    #[test]
    fn a_real_signal_requests_shutdown() {
        // SIGUSR1 keeps the test harmless: it is not SIGTERM/SIGINT.
        let s = Shutdown::install_for(&[signal_hook::consts::SIGUSR1]).unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGUSR1).unwrap();
        assert!(s.wait(Duration::from_secs(10)));
    }
}
