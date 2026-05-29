// Polling state machine: owns the connect/retry/sleep transitions and
// dispatches collected metrics to the Publisher.

use crate::adapter::{Adapter, AdapterError};
use crate::config::DaemonSection;
use crate::output::Publisher;
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Returns true if the cycle-health watchdog should fire.
pub fn watchdog_should_fire(count: u32, limit: u32) -> bool {
    limit > 0 && count >= limit
}

pub async fn run(
    mut adapter: Box<dyn Adapter>,
    daemon: DaemonSection,
    publisher: Publisher,
    running: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let poll_interval = Duration::from_secs_f32(daemon.poll_interval_secs);
    let sleep_interval = Duration::from_secs_f32(daemon.car_sleep_interval_secs);
    let mut poll_deadline = Instant::now();
    let mut no_publish_cycles: u32 = 0;
    let limit = daemon.cycle_failure_limit;

    'connect: loop {
        if !running.load(Ordering::SeqCst) {
            info!("shutdown requested");
            adapter.disconnect().await;
            break;
        }
        if let Err(e) = adapter.connect().await {
            info!("connect failed ({e}); retrying in {:?}", poll_interval);
            tokio::time::sleep(poll_interval).await;
            continue 'connect;
        }

        loop {
            if !running.load(Ordering::SeqCst) {
                adapter.disconnect().await;
                break 'connect;
            }
            if Instant::now() < poll_deadline {
                tokio::time::sleep(Duration::from_millis(30)).await;
                continue;
            }
            match adapter.poll().await {
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
}
