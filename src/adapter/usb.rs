// src/adapter/usb.rs
//
// USB ELM327 adapter — tokio-serial SerialStream wrapped in Elm327Session.
// Reuses the exact same protocol code as the Bluetooth adapter.

use super::{Adapter, AdapterError, Metrics};
use crate::adapter::elm327::{extract_value, Elm327Session, DEFAULT_INIT};
use crate::profile::{Source, VehicleProfile};
use anyhow::anyhow;
use async_trait::async_trait;
use log::{debug, info, warn};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

pub struct UsbElm327Adapter {
    port: String,
    baud: u32,
    profile: VehicleProfile,
    session: Option<Elm327Session<SerialStream>>,
}

impl UsbElm327Adapter {
    pub fn new(port: &str, baud: u32, profile: VehicleProfile) -> anyhow::Result<Self> {
        Ok(Self { port: port.into(), baud, profile, session: None })
    }
}

#[async_trait]
impl Adapter for UsbElm327Adapter {
    async fn connect(&mut self) -> Result<(), AdapterError> {
        info!("Opening USB serial port {} @ {} baud", self.port, self.baud);
        let stream = tokio_serial::new(&self.port, self.baud)
            .open_native_async()
            .map_err(|e| AdapterError::FatalConn(anyhow!("open {}: {e}", self.port)))?;

        let init: Vec<String> = self.profile.elm327.as_ref()
            .and_then(|e| e.init.clone())
            .unwrap_or_else(|| DEFAULT_INIT.iter().map(|s| s.to_string()).collect());
        let init_refs: Vec<&str> = init.iter().map(String::as_str).collect();

        let mut session = Elm327Session::new(stream);
        session.run_init(&init_refs).await.map_err(AdapterError::FatalConn)?;
        self.session = Some(session);
        info!("USB ELM327 initialised");
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
                        Ok(m)  => metrics.extend(m),
                        Err(e) => warn!("broadcast failed: {e:#}"),
                    }
                }
            }
        }
        Ok(metrics)
    }
}
