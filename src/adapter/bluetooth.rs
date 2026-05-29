// Bluetooth ELM327 adapter: an RFCOMM stream driven through Elm327Session.

use super::{Adapter, AdapterError, Metrics};
use crate::adapter::elm327::{extract_value, Elm327Session, DEFAULT_INIT};
use crate::profile::{Source, VehicleProfile};
use anyhow::anyhow;
use async_trait::async_trait;
use bluer::{rfcomm::{SocketAddr, Stream}, Address};
use log::{debug, info, warn};
use std::time::Duration;
use tokio::time::timeout;

pub struct BluetoothElm327Adapter {
    target: SocketAddr,
    profile: VehicleProfile,
    session: Option<Elm327Session<Stream>>,
    bt_passkey: Option<u32>,
    max_retries: u8,
    timeout_secs: u8,
    bluer_session: Option<bluer::Session>,
    _agent_handle: Option<bluer::agent::AgentHandle>,
}

impl BluetoothElm327Adapter {
    pub fn new(
        mac: &str,
        profile: VehicleProfile,
        bt_passkey: Option<u32>,
        max_retries: u8,
        timeout_secs: u8,
    ) -> anyhow::Result<Self> {
        let addr: Address = mac.parse()
            .map_err(|_| anyhow!("invalid bluetooth MAC '{mac}'"))?;
        Ok(Self {
            target: SocketAddr::new(addr, 1u8),
            profile,
            session: None,
            bt_passkey,
            max_retries: max_retries.max(1),
            timeout_secs,
            bluer_session: None,
            _agent_handle: None,
        })
    }
}

#[async_trait]
impl Adapter for BluetoothElm327Adapter {
    async fn connect(&mut self) -> Result<(), AdapterError> {
        // Register the pairing agent once and keep it for the life of the
        // adapter; re-registering on every reconnect would repeatedly claim the
        // system default agent.
        if let (Some(pk), None) = (self.bt_passkey, self._agent_handle.as_ref()) {
            let session = bluer::Session::new().await
                .map_err(|e| AdapterError::FatalConn(anyhow!("bluer session: {e}")))?;
            let handle = crate::adapter::pairing::register_passkey_agent(&session, pk).await
                .map_err(AdapterError::FatalConn)?;
            self.bluer_session = Some(session);
            self._agent_handle = Some(handle);
        }
        let connect_timeout = Duration::from_secs(self.timeout_secs as u64);
        let mut connected = None;
        for attempt in 1..=self.max_retries {
            info!("Connecting to {:?} (attempt {}/{})", self.target, attempt, self.max_retries);
            match timeout(connect_timeout, Stream::connect(self.target)).await {
                Ok(Ok(s)) => { connected = Some(s); break; }
                Ok(Err(e)) => warn!("BT connect attempt {attempt} failed: {e}"),
                Err(_) => warn!("BT connect attempt {attempt} timed out after {}s", self.timeout_secs),
            }
            if attempt < self.max_retries {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        let stream = connected.ok_or_else(|| {
            AdapterError::FatalConn(anyhow!("BT connect failed after {} attempts", self.max_retries))
        })?;

        // bluer local-address workaround
        // https://github.com/bluez/bluer/discussions/130#discussioncomment-8845113
        let mut i = 0;
        while stream.as_ref().local_addr()
            .map(|a| a.addr == bluer::Address::any()).unwrap_or(false)
        {
            debug!("Waiting for local address...");
            tokio::time::sleep(Duration::from_secs(1)).await;
            i += 1;
            if i > 5 { break; }
        }

        let init: Vec<String> = self.profile.elm327.as_ref()
            .and_then(|e| e.init.clone())
            .unwrap_or_else(|| DEFAULT_INIT.iter().map(|s| s.to_string()).collect());
        let init_refs: Vec<&str> = init.iter().map(String::as_str).collect();

        let mut session = Elm327Session::new(stream);
        session.run_init(&init_refs).await
            .map_err(AdapterError::FatalConn)?;

        self.session = Some(session);
        info!("BT connected and ELM327 initialised");
        Ok(())
    }

    async fn poll(&mut self) -> Result<Metrics, AdapterError> {
        let session = self.session.as_mut()
            .ok_or_else(|| AdapterError::FatalConn(anyhow!("not connected")))?;

        let mut metrics = Metrics::new();
        for source in &self.profile.sources {
            match source {
                Source::UdsPid(uds) => {
                    debug!("Polling PID {} (multiframe={})", uds.pid, uds.multiframe);
                    let result = if uds.multiframe {
                        session.poll_uds_pid_multiframe(uds).await
                    } else {
                        session.poll_uds_pid(uds).await
                    };
                    match result {
                        Ok(payload) if payload.is_empty() => continue,
                        Ok(payload) => {
                            for f in &uds.fields {
                                if let Some(v) = extract_value(&payload, f) {
                                    info!("{}: {}", f.name, v);
                                    metrics.insert(f.name.clone(), v);
                                } else {
                                    warn!("extract failed for field '{}'", f.name);
                                }
                            }
                        }
                        Err(e) => {
                            use std::io::ErrorKind;
                            match e.downcast_ref::<std::io::Error>().map(|io| io.kind()) {
                                Some(ErrorKind::AddrNotAvailable) => return Err(AdapterError::Sleeping),
                                Some(ErrorKind::BrokenPipe)
                                | Some(ErrorKind::TimedOut)
                                | Some(ErrorKind::NotConnected) => {
                                    return Err(AdapterError::FatalConn(e));
                                }
                                _ => warn!("PID {} failed: {e:#}", uds.pid),
                            }
                        }
                    }
                }
                Source::Broadcast(bc) => {
                    match session.monitor_broadcast(bc).await {
                        Ok(m) => metrics.extend(m),
                        Err(e) => warn!("broadcast source failed: {e:#}"),
                    }
                }
            }
        }
        Ok(metrics)
    }
}
