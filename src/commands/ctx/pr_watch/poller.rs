//! Background PR/MR watch poller (#949): one `pr-watch` thread per supervised
//! session sweeps that session's watches through the same code as
//! `zirv ctx pr poll`, then waits. It never mails a failure, never panics and
//! never touches the session: any internal error ends the thread quietly.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use super::parse::Failure;
use super::run::poll_session;
use super::store;
use crate::commands::ctx::config::{CtxConfig, env_from_process};
use crate::commands::ctx::log;
use crate::commands::ctx::state::{StateDir, now_secs};

/// Longest wait after any failure.
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// How long `Drop` waits for the thread before letting it finish on its own.
const STOP_WAIT: Duration = Duration::from_secs(1);

type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct Poller {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Poller {
    /// `None` unless `[pr_watch]` and `[mail]` are both enabled (or the thread cannot start).
    pub fn start(
        cfg: &CtxConfig,
        _repo: &Path,
        state: &StateDir,
        owner_short: &str,
    ) -> Option<Poller> {
        let env: Env = Arc::new(|key: &str| env_from_process()(key));
        Self::start_with(
            cfg,
            state,
            owner_short,
            Duration::from_secs(cfg.pr_watch.interval_secs),
            env,
        )
    }

    fn start_with(
        cfg: &CtxConfig,
        state: &StateDir,
        owner_short: &str,
        interval: Duration,
        env: Env,
    ) -> Option<Poller> {
        if !cfg.pr_watch.enabled || !cfg.mail.enabled || owner_short.is_empty() {
            return None;
        }
        let (stop, rx) = mpsc::channel::<()>();
        let cfg = cfg.clone();
        let thread_state = state.clone();
        let owner = owner_short.to_string();
        let spawned = std::thread::Builder::new()
            .name("pr-watch".to_string())
            .spawn(move || run_loop(&cfg, &thread_state, &owner, interval, &env, &rx));
        match spawned {
            Ok(thread) => Some(Poller {
                stop: Some(stop),
                thread: Some(thread),
            }),
            Err(e) => {
                note(state, owner_short, "worker thread", &e.to_string());
                None
            }
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        // Closing the channel wakes the wait at once.
        drop(self.stop.take());
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + STOP_WAIT;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // A read still in flight finishes on its own; the process exit reaps it.
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}

fn run_loop(
    cfg: &CtxConfig,
    state: &StateDir,
    owner: &str,
    interval: Duration,
    env: &Env,
    stop: &mpsc::Receiver<()>,
) {
    let mut delay = interval;
    let mut streak = 0u32;
    loop {
        if let Err(e) = poll_session(state, cfg, &**env, owner, &mut |_, _| Ok(())) {
            note(state, owner, "stopped", &e.to_string());
            return;
        }
        let worst = worst_failure(state, owner);
        streak = if worst.is_some() { streak + 1 } else { 0 };
        delay = next_delay(interval, delay, worst, streak);
        match stop.recv_timeout(jitter(delay)) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The most backoff-worthy failure still standing on this session's live watches.
fn worst_failure(state: &StateDir, owner: &str) -> Option<Failure> {
    let rank = |f: Failure| match f {
        Failure::NoCli | Failure::Auth => 3,
        Failure::RateLimited => 2,
        Failure::Transient => 1,
        // Per-watch outcomes that end the watch themselves; no reason to slow the others.
        Failure::NotFound | Failure::UntrustedHost => 0,
    };
    store::list(state, owner)
        .into_iter()
        .filter(|rec| rec.stopped.is_none())
        .filter_map(|rec| rec.last_error.map(|e| e.kind))
        .filter(|kind| rank(*kind) > 0)
        .max_by_key(|kind| rank(*kind))
}

/// The wait before the next sweep: `interval` when healthy, else the design's backoff, never above 30 min or `interval`, whichever is longer.
fn next_delay(
    interval: Duration,
    previous: Duration,
    worst: Option<Failure>,
    streak: u32,
) -> Duration {
    let Some(failure) = worst else {
        return interval;
    };
    let cap = MAX_BACKOFF.max(interval);
    let delay = match failure {
        Failure::NoCli | Failure::Auth => cap,
        Failure::RateLimited => previous.max(interval).saturating_mul(2),
        _ => interval.saturating_mul(2u32.saturating_pow(streak.min(16))),
    };
    delay.min(cap)
}

/// Up to 20% shorter, from the clock's sub-second noise; spreads sessions that started together.
fn jitter(delay: Duration) -> Duration {
    let noise = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() % 1000);
    delay - (delay / 5).saturating_mul(noise) / 1000
}

fn note(state: &StateDir, owner: &str, action: &str, detail: &str) {
    let _ = log::append(
        state,
        &log::Decision {
            ts: now_secs(),
            session: owner,
            verb: "pr-watch",
            verdict: "n/a",
            score: 0,
            action,
            detail,
            observed_at: None,
        },
    );
}

/// `zirv ctx status` line: watches across all sessions and what is wrong with them; `None` while the feature is off.
pub fn status_line(cfg: &CtxConfig, state: &StateDir) -> Option<String> {
    if !cfg.pr_watch.enabled {
        return None;
    }
    let owners = std::fs::read_dir(state.pr_watch()).ok();
    let records: Vec<_> = owners
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|dir| dir.file_name().to_str().map(str::to_string))
        .flat_map(|owner| store::list(state, &owner))
        .collect();
    let mut line = format!("{} watch(es)", records.len());
    let mut kinds: Vec<&str> = records
        .iter()
        .filter_map(|rec| rec.last_error.as_ref().map(|e| e.kind.as_str()))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    if !kinds.is_empty() {
        line.push_str(&format!(", last error: {}", kinds.join(", ")));
    }
    let mut stopped: Vec<&str> = records
        .iter()
        .filter_map(|rec| rec.stopped.as_deref())
        .collect();
    stopped.sort_unstable();
    stopped.dedup();
    if !stopped.is_empty() {
        line.push_str(&format!(", stopped: {}", stopped.join(", ")));
    }
    Some(line)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::super::run::tests::{GH_URL, OWNER, Rig};
    use super::*;

    const FAST: Duration = Duration::from_millis(50);

    fn config(rig: &Rig) -> CtxConfig {
        CtxConfig::load(&rig.repo, &rig.lookup()).expect("config")
    }

    fn start(rig: &Rig, cfg: &CtxConfig) -> Option<Poller> {
        let env = rig.env.clone();
        Poller::start_with(
            cfg,
            &rig.state(),
            OWNER,
            FAST,
            Arc::new(move |key: &str| env.get(key).cloned()),
        )
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_started_poller_mails_a_wake_for_failing_checks() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view.json");
        rig.watch(GH_URL);
        let poller = start(&rig, &config(&rig)).expect("enabled");
        wait_for("the wake mail", || !rig.mails().is_empty());
        drop(poller);
        let mails = rig.mails();
        assert_eq!(mails.len(), 1);
        assert!(
            mails[0].1.body.contains("Checks failed"),
            "{}",
            mails[0].1.body
        );
        assert_eq!(mails[0].1.to_session.as_deref(), Some(OWNER));
    }

    #[test]
    fn drop_returns_promptly_while_the_poller_waits() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view.json");
        rig.watch(GH_URL);
        let cfg = config(&rig);
        let env = rig.env.clone();
        let poller = Poller::start_with(
            &cfg,
            &rig.state(),
            OWNER,
            Duration::from_secs(3600),
            Arc::new(move |key: &str| env.get(key).cloned()),
        )
        .expect("enabled");
        wait_for("the first sweep", || !rig.mails().is_empty());
        let began = Instant::now();
        drop(poller);
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn two_pollers_for_one_owner_send_one_wake() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view.json");
        rig.watch(GH_URL);
        let cfg = config(&rig);
        let (first, second) = (start(&rig, &cfg), start(&rig, &cfg));
        wait_for("the wake mail", || !rig.mails().is_empty());
        std::thread::sleep(Duration::from_millis(600));
        drop((first, second));
        assert_eq!(rig.mails().len(), 1);
        let rec = store::load(&rig.state(), OWNER, GH_URL).expect("record");
        assert_eq!(rec.wake_seq, 1);
    }

    #[test]
    fn a_missing_cli_records_no_cli_mails_nothing_and_backs_off() {
        let rig = Rig::new();
        rig.watch(GH_URL);
        let poller = start(&rig, &config(&rig)).expect("enabled");
        wait_for("no_cli", || {
            store::load(&rig.state(), OWNER, GH_URL).is_some_and(|r| r.last_error.is_some())
        });
        std::thread::sleep(Duration::from_millis(500));
        drop(poller);
        let rec = store::load(&rig.state(), OWNER, GH_URL).expect("record");
        assert_eq!(rec.last_error.map(|e| e.kind), Some(Failure::NoCli));
        assert_eq!(
            rec.consecutive_failures, 1,
            "backed off: no second read at 50 ms"
        );
        assert!(rig.mails().is_empty());
    }

    #[test]
    fn backoff_follows_the_failure_kind_and_caps_at_thirty_minutes() {
        let base = Duration::from_secs(120);
        assert_eq!(next_delay(base, base, None, 0), base);
        assert_eq!(next_delay(base, base, Some(Failure::NoCli), 1), MAX_BACKOFF);
        assert_eq!(next_delay(base, base, Some(Failure::Auth), 1), MAX_BACKOFF);
        assert_eq!(
            next_delay(base, base, Some(Failure::Transient), 1),
            base * 2
        );
        assert_eq!(
            next_delay(base, base, Some(Failure::Transient), 3),
            base * 8
        );
        assert_eq!(
            next_delay(base, base, Some(Failure::Transient), 30),
            MAX_BACKOFF
        );
        assert_eq!(
            next_delay(base, base, Some(Failure::RateLimited), 1),
            base * 2
        );
        assert_eq!(
            next_delay(base, base * 2, Some(Failure::RateLimited), 2),
            base * 4
        );
        assert_eq!(
            next_delay(base, MAX_BACKOFF, Some(Failure::RateLimited), 9),
            MAX_BACKOFF
        );
        assert!(jitter(base) <= base && jitter(base) >= base * 4 / 5);
    }

    #[test]
    fn a_healthy_interval_longer_than_the_backoff_cap_is_kept() {
        let long = Duration::from_secs(7200);
        assert_eq!(next_delay(long, long, None, 0), long);
        assert_eq!(next_delay(long, long, Some(Failure::NoCli), 1), long);
        assert_eq!(next_delay(long, long, Some(Failure::Transient), 1), long);
        assert!(jitter(long) <= long && jitter(long) >= long * 4 / 5);
    }

    #[test]
    fn an_enormous_interval_never_panics() {
        let huge = Duration::from_secs(u64::MAX);
        for worst in [
            None,
            Some(Failure::NoCli),
            Some(Failure::Auth),
            Some(Failure::RateLimited),
            Some(Failure::Transient),
        ] {
            let delay = next_delay(huge, huge, worst, 16);
            assert!(delay >= MAX_BACKOFF, "{worst:?}");
            let _ = jitter(delay);
        }
    }

    #[test]
    fn the_poller_does_not_start_when_the_feature_or_mail_is_disabled() {
        let rig = Rig::new();
        let mut cfg = config(&rig);
        assert!(start(&rig, &cfg).is_some());
        cfg.mail.enabled = false;
        assert!(start(&rig, &cfg).is_none());
        cfg.mail.enabled = true;
        cfg.pr_watch.enabled = false;
        assert!(start(&rig, &cfg).is_none());
        assert!(Poller::start(&cfg, &rig.repo, &rig.state(), OWNER).is_none());
    }

    #[test]
    fn the_status_line_shows_watches_and_errors_only_when_enabled() {
        let rig = Rig::new();
        let mut cfg = config(&rig);
        assert_eq!(
            status_line(&cfg, &rig.state()).as_deref(),
            Some("0 watch(es)")
        );
        rig.watch(GH_URL);
        assert_eq!(
            status_line(&cfg, &rig.state()).as_deref(),
            Some("1 watch(es)")
        );
        assert!(rig.poll().contains("error: no_cli"));
        assert_eq!(
            status_line(&cfg, &rig.state()).as_deref(),
            Some("1 watch(es), last error: no_cli")
        );
        cfg.pr_watch.enabled = false;
        assert_eq!(status_line(&cfg, &rig.state()), None);
    }
}
