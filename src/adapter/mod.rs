// src/adapter/mod.rs
//
// Adapter trait + error taxonomy. Each transport (BT, USB, WiCAN) implements
// Adapter so the scheduler can treat them uniformly.

use std::collections::HashMap;

pub mod bluetooth;
pub mod elm327;
pub mod pairing;
pub mod usb;
pub mod wican;

pub type Metrics = HashMap<String, f32>;

/// What kind of error happened in poll/connect. The scheduler decides what
/// to do with each variant.
#[derive(Debug)]
pub enum AdapterError {
    /// Retry next cycle, keep connection.
    Transient(anyhow::Error),
    /// Reconnect from scratch.
    FatalConn(anyhow::Error),
    /// CAN bus down — extend poll interval.
    Sleeping,
    /// Log and exit 1.
    Permanent(anyhow::Error),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::Transient(e) => write!(f, "transient: {e:#}"),
            AdapterError::FatalConn(e) => write!(f, "fatal-conn: {e:#}"),
            AdapterError::Sleeping     => write!(f, "sleeping"),
            AdapterError::Permanent(e) => write!(f, "permanent: {e:#}"),
        }
    }
}

#[async_trait::async_trait]
pub trait Adapter: Send {
    async fn connect(&mut self) -> Result<(), AdapterError>;
    async fn poll(&mut self) -> Result<Metrics, AdapterError>;
    async fn disconnect(&mut self) {}
}
