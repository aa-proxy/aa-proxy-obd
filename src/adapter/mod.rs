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

#[cfg(test)]
pub mod elm327_emu;
#[cfg(test)]
mod replay_tests;

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
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::Transient(e) => write!(f, "transient: {e:#}"),
            AdapterError::FatalConn(e) => write!(f, "fatal-conn: {e:#}"),
            AdapterError::Sleeping     => write!(f, "sleeping"),
        }
    }
}

/// Map an error to the action the scheduler takes, using the first
/// `std::io::Error` found in the cause chain (context wrappers are skipped).
/// A link that is gone maps to `FatalConn`, a down CAN bus to `Sleeping`, and
/// everything else, including errors with no I/O cause, to `Transient`.
pub fn classify(e: anyhow::Error) -> AdapterError {
    use std::io::ErrorKind;
    let verdict = e
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(|io| match io.kind() {
            ErrorKind::AddrNotAvailable => Some(true),
            ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
            | ErrorKind::NotConnected
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::UnexpectedEof => Some(false),
            _ if is_host_gone(io) => Some(false),
            _ => None,
        });
    match verdict {
        Some(Some(true)) => AdapterError::Sleeping,
        Some(Some(false)) => AdapterError::FatalConn(e),
        _ => AdapterError::Transient(e),
    }
}

/// EHOSTDOWN (112) and EHOSTUNREACH (113) on Linux: the remote end of the
/// link is gone.
#[cfg(target_os = "linux")]
fn is_host_gone(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(112) | Some(113))
}

#[cfg(not(target_os = "linux"))]
fn is_host_gone(_: &std::io::Error) -> bool {
    false
}

#[async_trait::async_trait]
pub trait Adapter: Send {
    async fn connect(&mut self) -> Result<(), AdapterError>;
    async fn poll(&mut self) -> Result<Metrics, AdapterError>;
    async fn disconnect(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    fn kind_of(e: anyhow::Error) -> &'static str {
        match classify(e) {
            AdapterError::Transient(_) => "transient",
            AdapterError::FatalConn(_) => "fatal",
            AdapterError::Sleeping => "sleeping",
        }
    }

    #[test]
    fn bare_io_errors_map_by_kind() {
        let fatal = [
            ErrorKind::BrokenPipe,
            ErrorKind::TimedOut,
            ErrorKind::NotConnected,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::UnexpectedEof,
        ];
        for k in fatal {
            assert_eq!(kind_of(anyhow::Error::new(IoError::new(k, "x"))), "fatal", "{k:?}");
        }
        let e = anyhow::Error::new(IoError::new(ErrorKind::AddrNotAvailable, "x"));
        assert_eq!(kind_of(e), "sleeping");
        let e = anyhow::Error::new(IoError::new(ErrorKind::InvalidData, "x"));
        assert_eq!(kind_of(e), "transient");
    }

    #[test]
    fn io_error_under_context_keeps_its_kind() {
        let e = anyhow::Error::new(IoError::new(ErrorKind::BrokenPipe, "x"))
            .context("multiframe read")
            .context("outer");
        assert_eq!(kind_of(e), "fatal");
        let e = anyhow::Error::new(IoError::new(ErrorKind::AddrNotAvailable, "x")).context("c");
        assert_eq!(kind_of(e), "sleeping");
    }

    #[test]
    fn error_without_io_cause_is_transient() {
        assert_eq!(kind_of(anyhow::anyhow!("UDS error response: NO DATA")), "transient");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_down_and_unreachable_are_fatal() {
        for raw in [112, 113] {
            let e = anyhow::Error::new(IoError::from_raw_os_error(raw)).context("write");
            assert_eq!(kind_of(e), "fatal", "os error {raw}");
        }
        let e = anyhow::Error::new(IoError::from_raw_os_error(1));
        assert_eq!(kind_of(e), "transient");
    }
}
