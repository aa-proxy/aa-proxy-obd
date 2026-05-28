// src/adapter/pairing.rs
//
// In-process Bluetooth pairing agent. The caller MUST keep the returned
// AgentHandle alive for the duration it wants the agent active — dropping it
// unregisters the agent.

use anyhow::{Context, Result};
use bluer::agent::{Agent, AgentHandle};
use bluer::Session;
use log::info;

pub async fn register_passkey_agent(session: &Session, passkey: u32) -> Result<AgentHandle> {
    let agent = Agent {
        request_default: true,
        request_passkey: Some(Box::new(move |_path| {
            Box::pin(async move {
                info!("device requested a passkey; supplying configured value");
                Ok(passkey)
            })
        })),
        ..Default::default()
    };
    session.register_agent(agent).await.context("register passkey agent")
}
