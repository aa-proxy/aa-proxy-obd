// Polling state machine: owns the connect/retry/sleep transitions and
// dispatches collected metrics to the Publisher.

use crate::adapter::{Adapter, AdapterError};
use crate::config::DaemonSection;
use crate::output::Publisher;
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Cooperative shutdown signal. `trigger()` flips an AtomicBool (for cheap
/// synchronous polling) and wakes every waiter on the Notify (for async
/// `select!` against long-running operations). `wait()` resolves immediately
/// if shutdown has already been triggered, otherwise parks until it is.
pub struct Shutdown {
    running: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            notify: Notify::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn trigger(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Future that completes the moment `trigger()` has been (or is) called.
    /// Race-free: registers interest in the Notify before reading the bool so
    /// a trigger between the two cannot be missed.
    pub async fn wait(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_running() {
            return;
        }
        notified.await;
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns true if the cycle-health watchdog should fire.
pub fn watchdog_should_fire(count: u32, limit: u32) -> bool {
    limit > 0 && count >= limit
}

/// Upper bound for one `poll` call. A poll sends a handful of short ELM327
/// commands, so anything near this long means the link is stuck.
const POLL_TIMEOUT: Duration = Duration::from_secs(120);

/// Returned by `run` when the cycle-health watchdog gives up. The caller maps
/// it to a distinct process exit code.
#[derive(Debug)]
pub struct WatchdogTripped {
    pub cycles: u32,
}

impl std::fmt::Display for WatchdogTripped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "watchdog tripped after {} consecutive failed cycles",
            self.cycles
        )
    }
}

impl std::error::Error for WatchdogTripped {}

pub async fn run(
    mut adapter: Box<dyn Adapter>,
    daemon: DaemonSection,
    publisher: Publisher,
    shutdown: Arc<Shutdown>,
) -> anyhow::Result<()> {
    let poll_interval = Duration::try_from_secs_f32(daemon.poll_interval_secs).map_err(|e| {
        anyhow::anyhow!(
            "invalid daemon.poll_interval_secs {}: {e}",
            daemon.poll_interval_secs
        )
    })?;
    let sleep_interval =
        Duration::try_from_secs_f32(daemon.car_sleep_interval_secs).map_err(|e| {
            anyhow::anyhow!(
                "invalid daemon.car_sleep_interval_secs {}: {e}",
                daemon.car_sleep_interval_secs
            )
        })?;
    let mut poll_deadline = Instant::now();
    let mut no_publish_cycles: u32 = 0;
    let limit = daemon.cycle_failure_limit;
    // Bound for one connect call, taken from the adapter's own retry settings.
    let connect_bound = adapter.connect_bound();

    'connect: loop {
        if !shutdown.is_running() {
            info!("shutdown requested");
            adapter.disconnect().await;
            break;
        }
        let connect_res = tokio::select! {
            biased;
            _ = shutdown.wait() => { adapter.disconnect().await; break 'connect; }
            r = tokio::time::timeout(connect_bound, adapter.connect()) => r,
        };
        let connect_res = match connect_res {
            Ok(r) => r,
            Err(_) => {
                warn!("connect timed out after {:?}", connect_bound);
                adapter.disconnect().await;
                Err(AdapterError::FatalConn(anyhow::anyhow!(
                    "connect timed out"
                )))
            }
        };
        if let Err(e) = connect_res {
            info!("connect failed ({e}); retrying in {:?}", poll_interval);
            tokio::select! {
                biased;
                _ = shutdown.wait() => { adapter.disconnect().await; break 'connect; }
                _ = tokio::time::sleep(poll_interval) => {}
            }
            continue 'connect;
        }

        loop {
            if !shutdown.is_running() {
                adapter.disconnect().await;
                break 'connect;
            }
            let now = Instant::now();
            if now < poll_deadline {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => { adapter.disconnect().await; break 'connect; }
                    _ = tokio::time::sleep(poll_deadline - now) => {}
                }
                continue;
            }
            let poll_res = tokio::select! {
                biased;
                _ = shutdown.wait() => { adapter.disconnect().await; break 'connect; }
                r = tokio::time::timeout(POLL_TIMEOUT, adapter.poll()) => r,
            };
            let poll_res = match poll_res {
                Ok(r) => r,
                Err(_) => {
                    warn!("poll timed out after {:?}", POLL_TIMEOUT);
                    Err(AdapterError::FatalConn(anyhow::anyhow!("poll timed out")))
                }
            };
            match poll_res {
                Ok(metrics) => {
                    // Drive the watchdog from whether the adapter produced data,
                    // not from publish success: a healthy adapter whose endpoint
                    // is down (circuit breaker open) should keep running and let
                    // the breaker back off, not be killed by the watchdog.
                    let produced_data = !metrics.is_empty();
                    info!("collected {} metrics", metrics.len());
                    let _ = publisher.publish(&metrics).await;
                    if produced_data {
                        no_publish_cycles = 0;
                    } else {
                        no_publish_cycles += 1;
                        if watchdog_should_fire(no_publish_cycles, limit) {
                            error!(
                                "watchdog tripped: {} consecutive cycles produced no data; exiting",
                                no_publish_cycles
                            );
                            adapter.disconnect().await;
                            return Err(WatchdogTripped {
                                cycles: no_publish_cycles,
                            }
                            .into());
                        }
                        warn!(
                            "no data collected this cycle ({}/{})",
                            no_publish_cycles, limit
                        );
                    }
                    poll_deadline = Instant::now() + poll_interval;
                }
                Err(AdapterError::Transient(e)) => {
                    info!("transient: {e:#}");
                    no_publish_cycles += 1;
                    if watchdog_should_fire(no_publish_cycles, limit) {
                        error!("watchdog tripped on transient errors; exiting");
                        adapter.disconnect().await;
                        return Err(WatchdogTripped {
                            cycles: no_publish_cycles,
                        }
                        .into());
                    }
                    poll_deadline = Instant::now() + poll_interval;
                }
                Err(AdapterError::FatalConn(e)) => {
                    info!(
                        "connection lost: {e:#}; reconnecting in {:?}",
                        poll_interval
                    );
                    adapter.disconnect().await;
                    tokio::select! {
                        biased;
                        _ = shutdown.wait() => break 'connect,
                        _ = tokio::time::sleep(poll_interval) => {}
                    }
                    continue 'connect;
                }
                Err(AdapterError::Sleeping) => {
                    info!("car asleep; long-poll {:?}", sleep_interval);
                    publisher.on_sleeping().await;
                    no_publish_cycles = 0;
                    poll_deadline = Instant::now() + sleep_interval;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_fires_at_or_above_limit() {
        assert!(!watchdog_should_fire(0, 20));
        assert!(!watchdog_should_fire(19, 20));
        assert!(watchdog_should_fire(20, 20));
        assert!(watchdog_should_fire(21, 20));
    }

    #[test]
    fn watchdog_disabled_when_limit_zero() {
        assert!(!watchdog_should_fire(9999, 0));
    }

    #[tokio::test]
    async fn shutdown_wait_returns_immediately_after_trigger() {
        let s = Shutdown::new();
        s.trigger();
        // Should resolve essentially instantly; the test framework's default
        // timeout will catch a hang.
        s.wait().await;
        assert!(!s.is_running());
    }

    use crate::adapter::Metrics;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    enum Step {
        Fatal,
        Empty,
        Never,
    }

    type Events = Arc<Mutex<Vec<(&'static str, Instant)>>>;

    struct Mock {
        events: Events,
        polls: VecDeque<Step>,
        connect_never: bool,
    }

    #[async_trait::async_trait]
    impl Adapter for Mock {
        async fn connect(&mut self) -> Result<(), AdapterError> {
            self.events
                .lock()
                .unwrap()
                .push(("connect", Instant::now()));
            if self.connect_never {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn poll(&mut self) -> Result<Metrics, AdapterError> {
            self.events.lock().unwrap().push(("poll", Instant::now()));
            match self.polls.pop_front().unwrap_or(Step::Never) {
                Step::Fatal => Err(AdapterError::FatalConn(anyhow::anyhow!("link lost"))),
                Step::Empty => Ok(Metrics::new()),
                Step::Never => std::future::pending().await,
            }
        }
        async fn disconnect(&mut self) {
            self.events
                .lock()
                .unwrap()
                .push(("disconnect", Instant::now()));
        }
    }

    fn harness(
        polls: Vec<Step>,
        connect_never: bool,
        limit: u32,
    ) -> (
        Events,
        Arc<Shutdown>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let mock = Mock {
            events: events.clone(),
            polls: polls.into(),
            connect_never,
        };
        let daemon = DaemonSection {
            poll_interval_secs: 10.0,
            cycle_failure_limit: limit,
            ..DaemonSection::default()
        };
        let publisher = Publisher::new("http://localhost", None, 5, 300, true).unwrap();
        let shutdown = Arc::new(Shutdown::new());
        let h = tokio::spawn(run(Box::new(mock), daemon, publisher, shutdown.clone()));
        (events, shutdown, h)
    }

    fn names(ev: &Events) -> Vec<&'static str> {
        ev.lock().unwrap().iter().map(|e| e.0).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn lost_links_are_paced_and_do_not_trip_the_watchdog() {
        let (events, sd, h) = harness(vec![Step::Fatal; 40], false, 20);
        tokio::time::sleep(Duration::from_secs(305)).await;
        assert!(!h.is_finished(), "lost links must not end the run");
        sd.trigger();
        h.await.unwrap().expect("clean shutdown is Ok");
        let ev = events.lock().unwrap();
        let connects: Vec<Instant> = ev
            .iter()
            .filter(|e| e.0 == "connect")
            .map(|e| e.1)
            .collect();
        assert!(connects.len() > 20, "{}", connects.len());
        for w in connects.windows(2) {
            assert!(w[1] - w[0] >= Duration::from_secs(10), "{:?}", w[1] - w[0]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn consecutive_empty_polls_trip_the_watchdog() {
        let (events, _sd, h) = harness(vec![Step::Empty; 40], false, 20);
        let err = h.await.unwrap().expect_err("watchdog must end the run");
        assert!(err.downcast_ref::<WatchdogTripped>().is_some(), "{err:#}");
        let ev = events.lock().unwrap();
        assert_eq!(ev.iter().filter(|e| e.0 == "poll").count(), 20);
        assert_eq!(ev.last().unwrap().0, "disconnect");
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_poll_is_cut_off_and_reconnected() {
        let (events, sd, h) = harness(vec![Step::Never, Step::Never], false, 20);
        tokio::time::sleep(Duration::from_secs(135)).await;
        sd.trigger();
        h.await.unwrap().unwrap();
        let ev = events.lock().unwrap();
        let t0 = ev[0].1;
        assert_eq!(
            ev.iter().map(|e| e.0).collect::<Vec<_>>(),
            [
                "connect",
                "poll",
                "disconnect",
                "connect",
                "poll",
                "disconnect"
            ]
        );
        assert_eq!(ev[2].1 - t0, POLL_TIMEOUT);
        assert!(ev[3].1 - ev[2].1 >= Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_connect_is_cut_off_at_the_bound() {
        let (events, sd, h) = harness(vec![], true, 20);
        tokio::time::sleep(Duration::from_secs(299)).await;
        assert_eq!(names(&events), ["connect"]);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(names(&events), ["connect", "disconnect"]);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(names(&events), ["connect", "disconnect", "connect"]);
        sd.trigger();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_a_long_poll_ends_promptly() {
        let (events, sd, h) = harness(vec![Step::Never], false, 20);
        tokio::time::sleep(Duration::from_secs(5)).await;
        let t = Instant::now();
        sd.trigger();
        h.await.unwrap().expect("clean shutdown is Ok");
        assert!(t.elapsed() < Duration::from_secs(1));
        assert_eq!(names(&events), ["connect", "poll", "disconnect"]);
    }

    #[tokio::test]
    async fn shutdown_wait_resolves_when_triggered_concurrently() {
        let s = Arc::new(Shutdown::new());
        let s2 = s.clone();
        let waiter = tokio::spawn(async move {
            s2.wait().await;
        });
        // Give the waiter a tick to register interest.
        tokio::task::yield_now().await;
        s.trigger();
        waiter.await.unwrap();
    }
}
