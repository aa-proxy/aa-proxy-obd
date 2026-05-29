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
use tokio::time::{self, timeout};

const WICAN_NOTIFY_UUID: Uuid = Uuid::from_u128(0x0200dec0_01ef_bc9a_5678_1234deadf0be);
const WICAN_WRITE_UUID:  Uuid = Uuid::from_u128(0x0300dec0_01ef_bc9a_5678_1234deadf0be);

#[derive(Debug, Deserialize)]
struct WicanResponse {
    #[serde(alias = "SOC")]   soc:   f32,
    #[serde(alias = "SOC_D")] soc_d: Option<f32>,
    #[serde(alias = "TMP_A")] outdoor_temperature: Option<f32>,
}

pub struct WicanAdapter {
    mac: Address,
    passkey: Option<u32>,
    max_retries: u8,
    timeout_secs: u8,
    session: Option<Session>,
    adapter: Option<BluerAdapter>,
    device: Option<Device>,
    _agent_handle: Option<bluer::agent::AgentHandle>,
}

impl WicanAdapter {
    pub fn new(mac: &str, passkey: Option<u32>, max_retries: u8, timeout_secs: u8) -> anyhow::Result<Self> {
        let addr: Address = mac.parse().map_err(|_| anyhow!("invalid WiCAN MAC '{mac}'"))?;
        Ok(Self {
            mac: addr, passkey, max_retries, timeout_secs,
            session: None, adapter: None, device: None, _agent_handle: None,
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
            self._agent_handle = Some(register_passkey_agent(session, pk).await?);
        }
        device.pair().await.context("pair failed")
    }

    async fn try_connect(&self, device: &Device) -> anyhow::Result<()> {
        if device.is_connected().await? { return Ok(()); }
        for attempt in 0..self.max_retries {
            match device.connect().await {
                Ok(_) => { info!("WiCAN connected"); return Ok(()); }
                Err(e) if attempt + 1 < self.max_retries => {
                    warn!("WiCAN connect attempt {}/{} failed: {e}", attempt + 1, self.max_retries);
                    time::sleep(Duration::from_secs(10)).await;
                }
                Err(e) => {
                    if let Some(a) = &self.adapter { let _ = a.remove_device(device.address()).await; }
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
        let device = self.device.as_ref().context("device missing")?;
        let (notify_ch, write_ch) = Self::find_chars(device).await?;

        let mut stream = Box::pin(notify_ch.notify().await?);
        write_ch.write(b"autopid -d\n").await?;
        info!("WiCAN autopid request sent; waiting up to {}s", self.timeout_secs);

        let mut buf: Vec<u8> = Vec::new();
        let start = time::Instant::now();
        let total = Duration::from_secs(self.timeout_secs as u64);
        loop {
            if start.elapsed() >= total {
                warn!("WiCAN response timed out after {}s", self.timeout_secs);
                break;
            }
            let remaining = total - start.elapsed();
            tokio::select! {
                _ = time::sleep(remaining) => break,
                next = stream.next() => match next {
                    Some(b) => {
                        buf.extend_from_slice(&b);
                        if let Ok(s) = std::str::from_utf8(&buf) {
                            if s.trim().ends_with('}') { break; }
                        }
                    }
                    None => break,
                }
            }
        }
        if buf.is_empty() { return Err(anyhow!("empty WiCAN response")); }

        let s = String::from_utf8(buf).context("WiCAN response not utf-8")?;
        let resp: WicanResponse = serde_json::from_str(s.trim())
            .with_context(|| format!("WiCAN JSON parse failed: '{s}'"))?;
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
        self.session = Some(session);
        self.adapter = Some(adapter);

        let device = self.find_device().await.map_err(AdapterError::FatalConn)?;
        self.try_pair(&device).await.map_err(AdapterError::FatalConn)?;
        self.try_connect(&device).await.map_err(AdapterError::FatalConn)?;
        self.device = Some(device);
        Ok(())
    }

    async fn poll(&mut self) -> Result<Metrics, AdapterError> {
        match self.fetch_metrics().await {
            Ok(m) => Ok(m),
            Err(e) => {
                // A malformed JSON burst is transient (the next poll may parse);
                // anything else is treated as a lost connection. Classify on the
                // error type, not its message text.
                if e.downcast_ref::<serde_json::Error>().is_some() {
                    Err(AdapterError::Transient(e))
                } else {
                    Err(AdapterError::FatalConn(e))
                }
            }
        }
    }

    async fn disconnect(&mut self) {
        if let Some(d) = &self.device {
            let _ = d.disconnect().await;
        }
        self.device = None;
        self._agent_handle = None;
    }
}
