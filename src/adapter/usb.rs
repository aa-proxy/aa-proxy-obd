// src/adapter/usb.rs
//
// USB ELM327 adapter — tokio-serial SerialStream wrapped in Elm327Session.
// Reuses the exact same protocol code as the Bluetooth adapter.

use super::{Adapter, AdapterError, Metrics};
use crate::adapter::elm327::{poll_sources, Elm327Session, DEFAULT_INIT};
use crate::profile::VehicleProfile;
use anyhow::anyhow;
use async_trait::async_trait;
use log::info;
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

        poll_sources(session, &self.profile).await
    }
}
