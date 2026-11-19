//! In-memory remote cleanup; never owns the local tunnel or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Confirmed,
    AuthenticationRequired,
    PermissionDenied,
    Transient,
    MissingId,
    Unconfirmed,
}

pub(super) fn classify(success: bool, stdout: &[u8], stderr: &[u8], session: &str) -> Outcome {
    if stdout.len() > 16 * 1024 || stderr.len() > 16 * 1024 {
        return Outcome::Unconfirmed;
    }
    if success {
        return if super::cleanup_response(stdout, session) {
            Outcome::Confirmed
        } else {
            Outcome::Unconfirmed
        };
    }
    let Ok(error) = std::str::from_utf8(stderr) else {
        return Outcome::Unconfirmed;
    };
    let error = error.trim();
    if let Some(rest) = error.strip_prefix("An error occurred (")
        && let Some((code, _)) = rest.split_once(") when calling the TerminateSession operation:")
    {
        return match code {
            "ExpiredToken"
            | "ExpiredTokenException"
            | "InvalidClientTokenId"
            | "UnrecognizedClientException"
            | "InvalidIdentityToken" => Outcome::AuthenticationRequired,
            "AccessDenied" | "AccessDeniedException" | "UnauthorizedOperation" => {
                Outcome::PermissionDenied
            }
            "Throttling"
            | "ThrottlingException"
            | "TooManyRequestsException"
            | "RequestTimeout"
            | "RequestTimeoutException"
            | "InternalServerError"
            | "InternalServerErrorException"
            | "ServiceUnavailable"
            | "ServiceUnavailableException" => Outcome::Transient,
            _ => Outcome::Unconfirmed,
        };
    }
    if error.starts_with("Unable to locate credentials.")
        || error.starts_with("Error loading SSO Token:")
        || error.starts_with("Token has expired and refresh failed")
        || error.starts_with(
            "Error when retrieving token from sso: Token has expired and refresh failed",
        )
        || error.starts_with(
            "The SSO session associated with this profile has expired or is otherwise invalid",
        )
    {
        Outcome::AuthenticationRequired
    } else if error.starts_with("Could not connect to the endpoint URL:")
        || error.starts_with("Connect timeout on endpoint URL:")
        || error.starts_with("Read timeout on endpoint URL:")
        || error.starts_with(
            "Connection was closed before we received a valid response from endpoint URL:",
        )
    {
        Outcome::Transient
    } else {
        Outcome::Unconfirmed
    }
}

use super::{CleanupResult, SsmTunnel};
use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Scheduled,
    Running,
    Paused,
    Exhausted,
    Stopped,
    Confirmed,
}

#[derive(Clone, Debug)]
pub struct Record {
    pub id: usize,
    pub result: CleanupResult,
    pub phase: Phase,
    pub dismissed: bool,
    pub can_retry: bool,
}

struct Pending {
    record: Record,
    config: Option<SsmTunnel>,
    worker: bool,
    stopped: bool,
}

type Runner = dyn Fn(&SsmTunnel, &str) -> Outcome + Send + Sync;
struct Inner {
    pending: Mutex<Vec<Pending>>,
    wake: Condvar,
    changed: tokio::sync::watch::Sender<()>,
    run: Box<Runner>,
    delays: [Duration; 3],
}

/// Shared by all windows for this app lifetime. Stores configuration, never credentials.
#[derive(Clone)]
pub struct Manager(Arc<Inner>);
impl Default for Manager {
    fn default() -> Self {
        Self::with_runner(
            super::terminate,
            [
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(15),
            ],
        )
    }
}
impl Manager {
    pub(crate) fn with_runner(
        run: impl Fn(&SsmTunnel, &str) -> Outcome + Send + Sync + 'static,
        delays: [Duration; 3],
    ) -> Self {
        Self(Arc::new(Inner {
            pending: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            changed: tokio::sync::watch::channel(()).0,
            run: Box::new(run),
            delays,
        }))
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.0.changed.subscribe()
    }
    fn changed(&self) {
        self.0.changed.send_replace(());
    }
    pub fn records(&self) -> Vec<Record> {
        self.0
            .pending
            .lock()
            .unwrap()
            .iter()
            .map(|p| {
                let mut record = p.record.clone();
                record.can_retry = !p.worker
                    && p.config.is_some()
                    && record.result.session.is_some()
                    && record.result.outcome != Outcome::MissingId
                    && record.phase != Phase::Confirmed;
                record
            })
            .collect()
    }
    pub fn add(&self, result: CleanupResult, mut config: Option<SsmTunnel>) {
        if result.outcome == Outcome::Confirmed {
            return;
        }
        if let Some(config) = &mut config {
            config.attempt = None;
        }
        let retry =
            result.outcome == Outcome::Transient && result.session.is_some() && config.is_some();
        let mut pending = self.0.pending.lock().unwrap();
        let id = pending.len();
        pending.push(Pending {
            record: Record {
                id,
                result,
                phase: if retry {
                    Phase::Scheduled
                } else {
                    Phase::Paused
                },
                dismissed: false,
                can_retry: false,
            },
            config,
            worker: retry,
            stopped: false,
        });
        drop(pending);
        self.changed();
        if retry {
            self.spawn(id, false);
        }
    }
    pub fn dismiss(&self, id: usize) {
        if let Some(p) = self.0.pending.lock().unwrap().get_mut(id) {
            p.record.dismissed = true;
        }
        self.changed();
    }
    pub fn reopen(&self) {
        for p in self.0.pending.lock().unwrap().iter_mut() {
            if p.record.phase != Phase::Confirmed {
                p.record.dismissed = false;
            }
        }
        self.changed();
    }
    pub fn retry(&self, id: usize) -> bool {
        let mut pending = self.0.pending.lock().unwrap();
        let Some(p) = pending.get_mut(id) else {
            return false;
        };
        if p.worker
            || p.config.is_none()
            || p.record.result.session.is_none()
            || p.record.result.outcome == Outcome::MissingId
            || p.record.phase == Phase::Confirmed
        {
            return false;
        }
        p.worker = true;
        p.stopped = false;
        p.record.dismissed = false;
        p.record.phase = Phase::Running;
        drop(pending);
        self.changed();
        self.spawn(id, true);
        true
    }
    pub fn stop(&self, id: usize) {
        if let Some(p) = self.0.pending.lock().unwrap().get_mut(id)
            && p.worker
        {
            p.stopped = true;
        }
        self.0.wake.notify_all();
        self.changed();
    }
    fn spawn(&self, id: usize, immediate: bool) {
        let manager = self.clone();
        std::thread::spawn(move || manager.work(id, immediate));
    }
    fn work(&self, id: usize, immediate: bool) {
        // The initial attempt already ran in Tunnel::drop. Manual Retry starts a fresh cycle.
        let delays = immediate
            .then_some(Duration::ZERO)
            .into_iter()
            .chain(self.0.delays);
        for delay in delays {
            let deadline = Instant::now() + delay;
            let mut pending = self.0.pending.lock().unwrap();
            pending[id].record.phase = if delay.is_zero() {
                Phase::Running
            } else {
                Phase::Scheduled
            };
            self.changed();
            while !pending[id].stopped && Instant::now() < deadline {
                pending = self
                    .0
                    .wake
                    .wait_timeout(pending, deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                    .0;
            }
            let p = &mut pending[id];
            if p.stopped {
                p.worker = false;
                p.record.phase = Phase::Stopped;
                drop(pending);
                self.changed();
                return;
            }
            p.record.phase = Phase::Running;
            let config = p.config.clone().expect("retry requires configuration");
            let session = p
                .record
                .result
                .session
                .clone()
                .expect("retry requires session ID");
            drop(pending);
            self.changed();
            let outcome = (self.0.run)(&config, &session);
            let mut pending = self.0.pending.lock().unwrap();
            let p = &mut pending[id];
            p.record.result.outcome = outcome;
            if outcome == Outcome::Confirmed || p.stopped || outcome != Outcome::Transient {
                p.worker = false;
                p.record.phase = if outcome == Outcome::Confirmed {
                    Phase::Confirmed
                } else if p.stopped {
                    Phase::Stopped
                } else {
                    Phase::Paused
                };
                drop(pending);
                self.changed();
                return;
            }
        }
        let mut pending = self.0.pending.lock().unwrap();
        let p = &mut pending[id];
        p.worker = false;
        p.record.phase = if p.stopped {
            Phase::Stopped
        } else {
            Phase::Exhausted
        };
        drop(pending);
        self.changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_classifies_only_recognized_failures() {
        for (diagnostic, expected) in [
            (
                "An error occurred (ExpiredTokenException) when calling the TerminateSession operation: secret",
                Outcome::AuthenticationRequired,
            ),
            (
                "An error occurred (AccessDeniedException) when calling the TerminateSession operation: secret",
                Outcome::PermissionDenied,
            ),
            (
                "An error occurred (ThrottlingException) when calling the TerminateSession operation: secret",
                Outcome::Transient,
            ),
            (
                "Could not connect to the endpoint URL: https://example.test",
                Outcome::Transient,
            ),
            (
                "Error when retrieving credentials from custom-process: AccessDeniedException network timeout",
                Outcome::Unconfirmed,
            ),
            ("arbitrary authentication timeout", Outcome::Unconfirmed),
        ] {
            assert_eq!(classify(false, b"", diagnostic.as_bytes(), "s-1"), expected);
        }
    }
    #[test]
    fn cleanup_confirmation_requires_success_and_matching_bounded_json() {
        let good = br#"{"SessionId":"s-1"}"#;
        assert_eq!(classify(true, good, b"", "s-1"), Outcome::Confirmed);
        for (success, stdout) in [
            (false, good.as_slice()),
            (true, br#"{"SessionId":"s-2"}"#.as_slice()),
            (true, b"garbage".as_slice()),
        ] {
            assert_eq!(classify(success, stdout, b"", "s-1"), Outcome::Unconfirmed);
        }
        assert_eq!(
            classify(true, &vec![b' '; 16385], b"", "s-1"),
            Outcome::Unconfirmed
        );
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn result(outcome: Outcome) -> super::super::CleanupResult {
        super::super::CleanupResult {
            outcome,
            session: Some("alice-123".into()),
            profile: Some("original".into()),
            region: Some("ap-northeast-1".into()),
        }
    }
    fn config() -> super::super::SsmTunnel {
        super::super::SsmTunnel {
            profile: Some("original".into()),
            ..Default::default()
        }
    }
    fn wait(manager: &Manager, predicate: impl Fn(&Record) -> bool) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if predicate(&manager.records()[0]) {
                return;
            }
            assert!(
                std::time::Instant::now() < until,
                "cleanup did not settle: {:?}",
                manager.records()
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    #[test]
    fn transient_retries_are_bounded_and_dismiss_does_not_abandon_them() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let manager = Manager::with_runner(
            move |config, session| {
                assert_eq!(config.profile.as_deref(), Some("original"));
                assert_eq!(session, "alice-123");
                count.fetch_add(1, Ordering::SeqCst);
                Outcome::Transient
            },
            [std::time::Duration::from_millis(2); 3],
        );
        manager.add(result(Outcome::Transient), Some(config()));
        manager.dismiss(0);
        wait(&manager, |r| r.phase == Phase::Exhausted);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(manager.records()[0].dismissed);
        manager.reopen();
        assert!(!manager.records()[0].dismissed);
        assert!(manager.retry(0));
        wait(&manager, |r| r.phase == Phase::Exhausted);
        assert_eq!(calls.load(Ordering::SeqCst), 7);
    }
    #[test]
    fn authentication_permission_unknown_and_missing_id_never_auto_retry() {
        let manager = Manager::with_runner(
            |_, _| panic!("unexpected retry"),
            [std::time::Duration::ZERO; 3],
        );
        for outcome in [
            Outcome::AuthenticationRequired,
            Outcome::PermissionDenied,
            Outcome::Unconfirmed,
            Outcome::MissingId,
        ] {
            let mut result = result(outcome);
            if outcome == Outcome::MissingId {
                result.session = None;
            }
            manager.add(result, Some(config()));
        }
        assert!(manager.records().iter().all(|r| r.phase == Phase::Paused));
        assert!(!manager.retry(3));
    }
    #[test]
    fn stop_wakes_scheduled_retry_and_success_wins_over_stop_in_flight() {
        let scheduled = Manager::with_runner(
            |_, _| panic!("stopped retry ran"),
            [std::time::Duration::from_secs(60); 3],
        );
        scheduled.add(result(Outcome::Transient), Some(config()));
        scheduled.stop(0);
        wait(&scheduled, |r| r.phase == Phase::Stopped);

        for outcome in [Outcome::Confirmed, Outcome::Transient] {
            let (started, start) = std::sync::mpsc::channel();
            let (finish, finished) = std::sync::mpsc::channel();
            let finished = std::sync::Mutex::new(finished);
            let manager = Manager::with_runner(
                move |_, _| {
                    started.send(()).unwrap();
                    finished.lock().unwrap().recv().unwrap();
                    outcome
                },
                [std::time::Duration::ZERO; 3],
            );
            manager.add(result(Outcome::AuthenticationRequired), Some(config()));
            assert!(manager.retry(0));
            start
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            assert!(!manager.retry(0));
            manager.stop(0);
            finish.send(()).unwrap();
            wait(&manager, |r| {
                r.phase
                    == if outcome == Outcome::Confirmed {
                        Phase::Confirmed
                    } else {
                        Phase::Stopped
                    }
            });
            assert_eq!(manager.records()[0].result.outcome, outcome);
        }
    }
}
