// Bluetooth ELM327 adapter: an RFCOMM stream driven through Elm327Session.

use super::{Adapter, AdapterError, Metrics};
use crate::adapter::elm327::{poll_sources, Elm327Session, DEFAULT_INIT};
use crate::profile::VehicleProfile;
use anyhow::anyhow;
use async_trait::async_trait;
use bluer::{rfcomm::{SocketAddr, Stream}, Address};
use log::{debug, info, warn};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        self.session.take();
        // Register the pairing agent once and keep it for the life of the
        // adapter; re-registering on every reconnect would repeatedly claim the
        // system default agent.
        if let (Some(pk), None) = (self.bt_passkey, self._agent_handle.as_ref()) {
            let session = bluer::Session::new().await
                .map_err(|e| AdapterError::FatalConn(anyhow!("bluer session: {e}")))?;
            let handle = crate::adapter::pairing::register_passkey_agent(&session, pk, self.target.addr).await
                .map_err(AdapterError::FatalConn)?;
            self.bluer_session = Some(session);
            self._agent_handle = Some(handle);
        }
        let connect_timeout = Duration::from_secs(u64::from(self.timeout_secs.max(1)));
        let mut connected: Option<Stream> = None;
        for attempt in 1..=self.max_retries {
            info!("Connecting to {} ch {} (attempt {}/{})",
                self.target.addr, self.target.channel, attempt, self.max_retries);
            let mut candidate = match timeout(connect_timeout, Stream::connect(self.target)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    warn!("BT connect attempt {attempt} failed: {e}");
                    if attempt < self.max_retries {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    continue;
                }
                Err(_) => {
                    warn!("BT connect attempt {attempt} timed out after {}s", self.timeout_secs);
                    if attempt < self.max_retries {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    continue;
                }
            };

            // bluer local-address workaround
            // https://github.com/bluez/bluer/discussions/130#discussioncomment-8845113
            let mut i = 0;
            while candidate.as_ref().local_addr()
                .map(|a| a.addr == bluer::Address::any()).unwrap_or(false)
            {
                debug!("Waiting for local address...");
                tokio::time::sleep(Duration::from_secs(1)).await;
                i += 1;
                if i > 5 {
                    warn!("local address still unset after {i} waits; continuing");
                    break;
                }
            }

            // RFCOMM connect() returns Ok as soon as BlueZ accepts the local
            // socket; remote-side reachability is only revealed by the first
            // I/O. Probe the link with a bare CR before declaring success so
            // a dead peer surfaces here, not as a misleading "init step 'ATZ'
            // failed" downstream.
            if let Err(e) = probe_rfcomm_link(&mut candidate).await {
                warn!("BT link probe attempt {attempt} failed: {e}");
                if attempt < self.max_retries {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                continue;
            }

            connected = Some(candidate);
            break;
        }
        let stream = connected.ok_or_else(|| {
            AdapterError::FatalConn(anyhow!("BT connect failed after {} attempts", self.max_retries))
        })?;

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

        poll_sources(session, &self.profile).await
    }

    async fn disconnect(&mut self) {
        self.session.take();
    }

    fn connect_bound(&self) -> Duration {
        super::retry_connect_bound(self.max_retries, self.timeout_secs)
    }
}

// Send a bare CR and wait for the reply. A live ELM327 answers with a '>'
// prompt (a lone CR repeats the last command; a freshly reset chip answers
// '?'); a dead peer fails the write immediately with HostDown/BrokenPipe
// /NotConnected, and a peer that closed the link reads as end-of-file. Read up
// to the prompt, or 2 s, so a late reply is not left
// in the stream to be taken as the answer to ATZ.
pub(crate) async fn probe_rfcomm_link<S>(stream: &mut S) -> std::io::Result<()>
where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    timeout(Duration::from_secs(2), stream.write_all(b"\r")).await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "probe write timed out"))??;
    let mut buf = [0u8; 256];
    let read = timeout(Duration::from_secs(2), async {
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed the link",
                ));
            }
            if buf[..n].contains(&b'>') {
                return Ok(());
            }
        }
    }).await;
    // No data within the window is not a failure; a read error or a closed
    // peer is.
    read.unwrap_or(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use tokio::io::AsyncReadExt;

    #[tokio::test(start_paused = true)]
    async fn probe_reports_a_peer_that_closed_the_link() {
        let (mut host, mut peer) = tokio::io::duplex(64);
        tokio::spawn(async move {
            let mut cr = [0u8; 1];
            peer.read_exact(&mut cr).await.unwrap();
            drop(peer);
        });
        let err = probe_rfcomm_link(&mut host).await.expect_err("closed peer must fail the probe");
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof, "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn probe_accepts_a_silent_peer() {
        let (mut host, _peer) = tokio::io::duplex(64);
        probe_rfcomm_link(&mut host).await.expect("a read timeout is not a failure");
    }
}
