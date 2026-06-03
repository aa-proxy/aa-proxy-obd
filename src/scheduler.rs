// Polling state machine: owns the connect/retry/sleep transitions and
// dispatches collected metrics to the Publisher.

use crate::adapter::{Adapter, AdapterError};
use crate::config::DaemonSection;
use crate::output::Publisher;
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Cooperative shutdown signal. `trigger()` flips an AtomicBool (for cheap
/// synchronous polling) and wakes every waiter on the Notify (for async
/// `select!` against long-running operations). `wait()` resolves immediately
/// if shutdown has already been triggered, otherwise parks until it is.
pub struct Shutdown {
    running: AtomicBool,
    notify:  Notify,
}

impl Shutdown {
    pub fn new() -> Self {
        Self { running: AtomicBool::new(true), notify: Notify::new() }
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
    fn default() -> Self { Self::new() }
}

/// Returns true if the cycle-health watchdog should fire.
pub fn watchdog_should_fire(count: u32, limit: u32) -> bool {
    limit > 0 && count >= limit
}

pub async fn run(
    mut adapter: Box<dyn Adapter>,
    daemon: DaemonSection,
    publisher: Publisher,
    shutdown: Arc<Shutdown>,
) -> anyhow::Result<()> {
    let poll_interval = Duration::from_secs_f32(daemon.poll_interval_secs);
    let sleep_interval = Duration::from_secs_f32(daemon.car_sleep_interval_secs);
    let mut poll_deadline = Instant::now();
    let mut no_publish_cycles: u32 = 0;
    let limit = daemon.cycle_failure_limit;

    'connect: loop {
        if !shutdown.is_running() {
            info!("shutdown requested");
            adapter.disconnect().await;
            break;
        }
        let connect_res = tokio::select! {
            biased;
            _ = shutdown.wait() => { adapter.disconnect().await; break 'connect; }
            r = adapter.connect() => r,
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
                r = adapter.poll() => r,
            };
            match poll_res {
                Ok(metrics) => {
                    // Drive the watchdog from whether the adapter produced data,
                    // not from publish success: a healthy adapter whose endpoint
                    // is down (circuit breaker open) should keep running and let
                    // the breaker back off, not be killed by the watchdog.
                    let produced_data = !metrics.is_empty();
                    let _ = publisher.publish(&metrics).await;
                    if produced_data {
                        no_publish_cycles = 0;
                    } else {
                        no_publish_cycles += 1;
                        if watchdog_should_fire(no_publish_cycles, limit) {
                            error!("watchdog tripped: {} consecutive cycles produced no data; exiting", no_publish_cycles);
                            std::process::exit(2);
                        }
                        warn!("no data collected this cycle ({}/{})", no_publish_cycles, limit);
                    }
                    poll_deadline = Instant::now() + poll_interval;
                }
                Err(AdapterError::Transient(e)) => {
                    info!("transient: {e:#}");
                    no_publish_cycles += 1;
                    if watchdog_should_fire(no_publish_cycles, limit) {
                        error!("watchdog tripped on transient errors; exiting");
                        std::process::exit(2);
                    }
                    poll_deadline = Instant::now() + poll_interval;
                }
                Err(AdapterError::FatalConn(e)) => {
                    info!("connection lost: {e:#}; reconnecting");
                    adapter.disconnect().await;
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

    #[tokio::test]
    async fn shutdown_wait_resolves_when_triggered_concurrently() {
        let s = Arc::new(Shutdown::new());
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.wait().await; });
        // Give the waiter a tick to register interest.
        tokio::task::yield_now().await;
        s.trigger();
        waiter.await.unwrap();
    }
}
