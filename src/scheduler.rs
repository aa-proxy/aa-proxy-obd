// Polling state machine: owns the connect/retry/sleep transitions and
// dispatches collected metrics to the Publisher.

use crate::adapter::{Adapter, AdapterError};
use crate::config::DaemonSection;
use crate::output::Publisher;
use log::{error, info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub async fn run(
    mut adapter: Box<dyn Adapter>,
    daemon: DaemonSection,
    publisher: Publisher,
    running: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let poll_interval = Duration::from_secs_f32(daemon.poll_interval_secs);
    let sleep_interval = Duration::from_secs_f32(daemon.car_sleep_interval_secs);
    let mut poll_deadline = Instant::now();

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
                    let _ok = publisher.publish(&metrics).await;
                    poll_deadline = Instant::now() + poll_interval;
                }
                Err(AdapterError::Transient(e)) => {
                    info!("transient: {e:#}");
                    poll_deadline = Instant::now() + poll_interval;
                }
                Err(AdapterError::FatalConn(e)) => {
                    info!("connection lost: {e:#}; reconnecting");
                    continue 'connect;
                }
                Err(AdapterError::Sleeping) => {
                    info!("car asleep; long-poll {:?}", sleep_interval);
                    poll_deadline = Instant::now() + sleep_interval;
                }
                Err(AdapterError::Permanent(e)) => {
                    error!("permanent: {e:#}");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}
