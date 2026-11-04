// WiCAN Pro adapter over BLE GATT. AutoPid mode emits a JSON blob on the
// notify characteristic in response to "autopid -d\n" on the write
// characteristic.

use super::{Adapter, AdapterError, Metrics};
use crate::adapter::pairing::register_passkey_agent;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use bluer::gatt::remote::Characteristic;
use bluer::{Adapter as BluerAdapter, AdapterEvent, Address, Device, Session, Uuid};
use futures_util::StreamExt;
use log::{debug, info, warn};
use serde::Deserialize;
use std::time::Duration;
use tokio::time::{self, timeout, Instant};

const WICAN_NOTIFY_UUID: Uuid = Uuid::from_u128(0x0200dec0_01ef_bc9a_5678_1234deadf0be);
const WICAN_WRITE_UUID:  Uuid = Uuid::from_u128(0x0300dec0_01ef_bc9a_5678_1234deadf0be);

#[derive(Debug, Deserialize)]
struct WicanResponse {
    #[serde(alias = "SOC")]   soc:   f32,
    #[serde(alias = "SOC_D")] soc_d: Option<f32>,
    #[serde(alias = "TMP_A")] outdoor_temperature: Option<f32>,
}

/// Outcome of parsing the bytes received so far.
#[derive(Debug)]
enum Frame {
    /// A full response object has arrived.
    Complete(WicanResponse),
    /// The object is not closed yet (or the bytes are not JSON); keep reading.
    Incomplete,
    /// A well-formed object that does not match the expected fields.
    Invalid(serde_json::Error),
}

/// Decide whether `buf` holds a full response. Only the first JSON value is
/// read: bytes before the first `{` are skipped and bytes after the value are
/// ignored. A value that the parser cannot finish yet is incomplete.
fn parse_frame(buf: &[u8]) -> Frame {
    let Some(start) = buf.iter().position(|&b| b == b'{') else {
        return Frame::Incomplete;
    };
    let mut values = serde_json::Deserializer::from_slice(&buf[start..]).into_iter::<WicanResponse>();
    match values.next() {
        Some(Ok(r)) => Frame::Complete(r),
        Some(Err(e)) if e.is_data() => Frame::Invalid(e),
        Some(Err(_)) | None => Frame::Incomplete,
    }
}

/// Time left before `deadline`; zero at or past it.
fn remaining(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

/// How a response parse failure should be handled.
#[derive(Debug, PartialEq, Eq)]
enum ParseClass {
    /// The response is well-formed but lacks or mangles an expected field;
    /// repeating the poll will not change that.
    Config,
    /// Truncated or malformed bytes; the next poll may parse.
    Transient,
}

fn classify_parse_error(e: &serde_json::Error) -> ParseClass {
    if e.is_data() { ParseClass::Config } else { ParseClass::Transient }
}

/// Bytes arrived but no full response did before the deadline.
#[derive(Debug)]
struct PartialResponse {
    received: usize,
    secs: u8,
}

impl std::fmt::Display for PartialResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WiCAN response incomplete after {}s ({} bytes received)", self.secs, self.received)
    }
}

impl std::error::Error for PartialResponse {}

/// Error for a read that reached its deadline: a partial response may
/// complete on the next poll, silence means the link is not delivering.
fn deadline_error(received: usize, secs: u8) -> anyhow::Error {
    if received == 0 {
        anyhow!("WiCAN sent no response within {secs}s")
    } else {
        anyhow::Error::new(PartialResponse { received, secs })
    }
}

/// What `poll` does with a failed read.
#[derive(Debug, PartialEq, Eq)]
enum PollClass {
    /// Fields missing or invalid in a complete response.
    Config,
    /// Retry on the next poll over the same connection.
    Transient,
    /// Reconnect.
    Fatal,
}

fn poll_class(e: &anyhow::Error) -> PollClass {
    if let Some(je) = e.downcast_ref::<serde_json::Error>() {
        return match classify_parse_error(je) {
            ParseClass::Config => PollClass::Config,
            ParseClass::Transient => PollClass::Transient,
        };
    }
    if e.downcast_ref::<PartialResponse>().is_some() {
        return PollClass::Transient;
    }
    PollClass::Fatal
}

/// BlueZ reports a stale or mismatched bond as a failed or rejected
/// authentication; removing the device then lets the next connect pair from
/// scratch. Timeout and cancel can happen with a valid key (slow or busy
/// device), so they keep the bond.
fn is_auth_failure(e: &bluer::Error) -> bool {
    matches!(
        e.kind,
        bluer::ErrorKind::AuthenticationFailed | bluer::ErrorKind::AuthenticationRejected
    )
}

pub struct WicanAdapter {
    mac: Address,
    passkey: Option<u32>,
    max_retries: u8,
    timeout_secs: u8,
    session: Option<Session>,
    adapter: Option<BluerAdapter>,
    device: Option<Device>,
    chars: Option<(Characteristic, Characteristic)>,
    config_warned: bool,
    _agent_handle: Option<bluer::agent::AgentHandle>,
}

impl WicanAdapter {
    pub fn new(mac: &str, passkey: Option<u32>, max_retries: u8, timeout_secs: u8) -> anyhow::Result<Self> {
        let addr: Address = mac.parse().map_err(|_| anyhow!("invalid WiCAN MAC '{mac}'"))?;
        Ok(Self {
            mac: addr, passkey, max_retries: max_retries.max(1), timeout_secs: timeout_secs.max(1),
            session: None, adapter: None, device: None, chars: None,
            config_warned: false, _agent_handle: None,
        })
    }

    async fn find_device(&self) -> anyhow::Result<Device> {
        let adapter = self.adapter.as_ref().context("adapter missing")?;
        // adapter.device(mac) only builds a handle from an address; it does not
        // indicate whether BlueZ knows the device. Check the known-address list
        // before deciding whether a discovery scan is needed.
        if adapter.device_addresses().await?.contains(&self.mac) {
            return Ok(adapter.device(self.mac)?);
        }
        let mut events = adapter.discover_devices().await?;
        let wait = Duration::from_secs(self.timeout_secs as u64);
        let outcome = timeout(wait, async {
            while let Some(ev) = events.next().await {
                if let AdapterEvent::DeviceAdded(addr) = ev {
                    if addr == self.mac { return Ok(adapter.device(addr)?); }
                }
            }
            Err(anyhow!("discovery stream ended"))
        }).await;
        match outcome {
            Ok(r) => r,
            Err(_) => Err(anyhow!("WiCAN discovery timed out after {}s", self.timeout_secs)),
        }
    }

    async fn try_pair(&mut self, device: &Device) -> anyhow::Result<()> {
        if device.is_paired().await? { return Ok(()); }
        if let Some(pk) = self.passkey {
            let session = self.session.as_ref().context("session missing")?;
            self._agent_handle = Some(register_passkey_agent(session, pk, self.mac).await?);
        }
        match device.pair().await {
            Ok(()) => Ok(()),
            Err(e) => {
                self.drop_bond_on_auth_failure(device, &e).await;
                Err(anyhow::Error::new(e).context("pair failed"))
            }
        }
    }

    async fn drop_bond_on_auth_failure(&self, device: &Device, e: &bluer::Error) {
        if !is_auth_failure(e) { return; }
        warn!("WiCAN authentication failed ({e}); removing the device so the next connect pairs again");
        if let Some(a) = &self.adapter {
            if let Err(re) = a.remove_device(device.address()).await {
                warn!("WiCAN device removal failed: {re}");
            }
        }
    }

    async fn try_connect(&self, device: &Device) -> anyhow::Result<()> {
        if device.is_connected().await? { return Ok(()); }
        for attempt in 0..self.max_retries {
            match device.connect().await {
                Ok(_) => { info!("WiCAN connected"); return Ok(()); }
                Err(e) if is_auth_failure(&e) => {
                    self.drop_bond_on_auth_failure(device, &e).await;
                    return Err(anyhow!("WiCAN connect rejected: {e}"));
                }
                Err(e) if attempt + 1 < self.max_retries => {
                    warn!("WiCAN connect attempt {}/{} failed: {e}", attempt + 1, self.max_retries);
                    time::sleep(Duration::from_secs(10)).await;
                }
                Err(e) => {
                    return Err(anyhow!("WiCAN connect exhausted retries: {e}"));
                }
            }
        }
        Err(anyhow!("WiCAN connect exhausted retries"))
    }

    async fn find_chars(device: &Device) -> anyhow::Result<(Characteristic, Characteristic)> {
        let mut notify: Option<Characteristic> = None;
        let mut write:  Option<Characteristic> = None;
        for svc in device.services().await? {
            for ch in svc.characteristics().await? {
                let uuid = ch.uuid().await?;
                if uuid == WICAN_NOTIFY_UUID { notify = Some(ch.clone()); }
                if uuid == WICAN_WRITE_UUID  { write  = Some(ch.clone()); }
            }
        }
        Ok((
            notify.ok_or_else(|| anyhow!("WiCAN notify characteristic missing"))?,
            write.ok_or_else(|| anyhow!("WiCAN write characteristic missing"))?,
        ))
    }

    async fn fetch_metrics(&self) -> anyhow::Result<Metrics> {
        let (notify_ch, write_ch) = self.chars.as_ref().context("characteristics not resolved")?;

        let mut stream = Box::pin(notify_ch.notify().await?);
        write_ch.write(b"autopid -d\n").await?;
        info!("WiCAN autopid request sent; waiting up to {}s", self.timeout_secs);

        let mut buf: Vec<u8> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs as u64);
        let resp = loop {
            if remaining(deadline, Instant::now()).is_zero() {
                warn!("WiCAN response timed out after {}s", self.timeout_secs);
                return Err(deadline_error(buf.len(), self.timeout_secs));
            }
            tokio::select! {
                _ = time::sleep_until(deadline) => {}
                next = stream.next() => match next {
                    Some(b) => {
                        buf.extend_from_slice(&b);
                        match parse_frame(&buf) {
                            Frame::Complete(r) => break r,
                            Frame::Invalid(e) => {
                                return Err(anyhow::Error::new(e).context(format!(
                                    "WiCAN JSON fields invalid: '{}'",
                                    String::from_utf8_lossy(&buf))));
                            }
                            Frame::Incomplete => {}
                        }
                    }
                    None => return Err(anyhow!("WiCAN notify stream ended after {} bytes", buf.len())),
                }
            }
        };
        debug!("WiCAN parsed: {resp:?}");

        let mut metrics = Metrics::new();
        metrics.insert("battery_level_percentage".into(), resp.soc_d.unwrap_or(resp.soc));
        if let Some(t) = resp.outdoor_temperature {
            metrics.insert("external_temp_celsius".into(), t);
        }
        Ok(metrics)
    }
}

#[async_trait]
impl Adapter for WicanAdapter {
    async fn connect(&mut self) -> Result<(), AdapterError> {
        let session = Session::new().await
            .map_err(|e| AdapterError::FatalConn(anyhow!("bluer session: {e}")))?;
        let adapter = session.default_adapter().await
            .map_err(|e| AdapterError::FatalConn(anyhow!("default adapter: {e}")))?;
        self.chars = None;
        self.config_warned = false;
        self.session = Some(session);
        self.adapter = Some(adapter);

        let device = self.find_device().await.map_err(AdapterError::FatalConn)?;
        self.try_pair(&device).await.map_err(AdapterError::FatalConn)?;
        self.try_connect(&device).await.map_err(AdapterError::FatalConn)?;
        let found = Self::find_chars(&device).await;
        self.device = Some(device);
        match found {
            Ok(c) => {
                self.chars = Some(c);
                Ok(())
            }
            Err(e) => {
                self.disconnect().await;
                Err(AdapterError::FatalConn(e))
            }
        }
    }

    async fn poll(&mut self) -> Result<Metrics, AdapterError> {
        match self.fetch_metrics().await {
            Ok(m) => Ok(m),
            Err(e) => {
                // Parse failures keep the connection: truncated bytes may parse
                // next time, and a field mismatch is logged once per connection.
                // Anything else is treated as a lost connection.
                match poll_class(&e) {
                    PollClass::Config => {
                        if !self.config_warned {
                            self.config_warned = true;
                            warn!("WiCAN response has a missing or invalid field ({e:#}); \
                                   check the adapter's AutoPid configuration");
                        }
                        Err(AdapterError::Transient(e))
                    }
                    PollClass::Transient => Err(AdapterError::Transient(e)),
                    PollClass::Fatal => Err(AdapterError::FatalConn(e)),
                }
            }
        }
    }

    async fn disconnect(&mut self) {
        if let Some(d) = &self.device {
            let _ = d.disconnect().await;
        }
        self.device = None;
        self.chars = None;
        self._agent_handle = None;
    }

    fn connect_bound(&self) -> Duration {
        super::wican_connect_bound(self.max_retries, self.timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_never_underflows() {
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(remaining(deadline, deadline - Duration::from_nanos(1)), Duration::from_nanos(1));
        assert_eq!(remaining(deadline, deadline), Duration::ZERO);
        assert_eq!(remaining(deadline, deadline + Duration::from_secs(3)), Duration::ZERO);
    }

    #[test]
    fn nested_close_at_chunk_boundary_is_incomplete() {
        let whole = br#"{"SOC": 55.5, "extra": {"a": 1}, "TMP_A": 7.0}"#;
        let cut = whole.iter().position(|&b| b == b'}').unwrap() + 1;
        assert!(matches!(parse_frame(&whole[..cut]), Frame::Incomplete));
        match parse_frame(whole) {
            Frame::Complete(r) => assert_eq!(r.soc, 55.5),
            other => panic!("expected complete, got {other:?}"),
        }
    }

    #[test]
    fn trailing_and_leading_bytes_do_not_hide_a_complete_object() {
        for buf in [
            &br#"{"SOC": 1.5}>"#[..],
            br#"{"SOC": 1.5}{"SOC": 2.5}"#,
            br#"> {"SOC": 1.5}"#,
            b"\x00\xffjunk\n{\"SOC\": 1.5}\r\n>",
        ] {
            match parse_frame(buf) {
                Frame::Complete(r) => assert_eq!(r.soc, 1.5),
                other => panic!("{:?}: {other:?}", String::from_utf8_lossy(buf)),
            }
        }
        assert!(matches!(parse_frame(br#"> {"SOC": 1."#), Frame::Incomplete));
    }

    #[test]
    fn deadline_with_partial_data_is_transient_and_silence_is_fatal() {
        assert_eq!(poll_class(&deadline_error(10, 4)), PollClass::Transient);
        assert_eq!(poll_class(&deadline_error(0, 4)), PollClass::Fatal);
        assert_eq!(poll_class(&anyhow!("notify stream ended")), PollClass::Fatal);
    }

    #[test]
    fn only_authentication_errors_drop_the_bond() {
        use bluer::{Error, ErrorKind};
        let mk = |kind| Error { kind, message: String::new() };
        assert!(is_auth_failure(&mk(ErrorKind::AuthenticationFailed)));
        assert!(is_auth_failure(&mk(ErrorKind::AuthenticationRejected)));
        assert!(!is_auth_failure(&mk(ErrorKind::AuthenticationTimeout)));
        assert!(!is_auth_failure(&mk(ErrorKind::AuthenticationCanceled)));
        assert!(!is_auth_failure(&mk(ErrorKind::Failed)));
        assert!(!is_auth_failure(&mk(ErrorKind::InProgress)));
    }

    #[test]
    fn missing_soc_is_config_and_truncated_is_transient() {
        let missing = serde_json::from_str::<WicanResponse>(r#"{"TMP_A": 3.0}"#).unwrap_err();
        assert_eq!(classify_parse_error(&missing), ParseClass::Config);
        assert!(matches!(parse_frame(br#"{"TMP_A": 3.0}"#), Frame::Invalid(_)));
        let cut = serde_json::from_str::<WicanResponse>(r#"{"SOC": 5"#).unwrap_err();
        assert_eq!(classify_parse_error(&cut), ParseClass::Transient);
    }
}
