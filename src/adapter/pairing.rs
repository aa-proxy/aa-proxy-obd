// src/adapter/pairing.rs
//
// In-process Bluetooth pairing agent. The caller MUST keep the returned
// AgentHandle alive for the duration it wants the agent active — dropping it
// unregisters the agent.

use anyhow::{bail, Context, Result};
use bluer::agent::{Agent, AgentHandle, ReqError};
use bluer::{Address, Session};
use log::{info, warn};

/// Largest value a Bluetooth passkey can take (six decimal digits).
const MAX_PASSKEY: u32 = 999_999;

fn validate_passkey(passkey: u32) -> Result<()> {
    if passkey > MAX_PASSKEY {
        bail!("passkey {passkey} is out of range; it must be at most {MAX_PASSKEY}");
    }
    Ok(())
}

/// The passkey to answer with, or `None` when the request comes from a device
/// other than the expected one.
fn passkey_for(requested: Address, expected: Address, passkey: u32) -> Option<u32> {
    (requested == expected).then_some(passkey)
}

/// Registers an agent that answers passkey requests from `device` only. The
/// callbacks left unset reject, because bluer rejects any request without a
/// handler.
pub async fn register_passkey_agent(
    session: &Session,
    passkey: u32,
    device: Address,
) -> Result<AgentHandle> {
    validate_passkey(passkey)?;
    let agent = Agent {
        request_default: true,
        request_passkey: Some(Box::new(move |req| {
            Box::pin(async move {
                match passkey_for(req.device, device, passkey) {
                    Some(pk) => {
                        info!(
                            "device {} requested a passkey; supplying configured value",
                            req.device
                        );
                        Ok(pk)
                    }
                    None => {
                        warn!(
                            "rejecting passkey request from unexpected device {}",
                            req.device
                        );
                        Err(ReqError::Rejected)
                    }
                }
            })
        })),
        ..Default::default()
    };
    session
        .register_agent(agent)
        .await
        .context("register passkey agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    const B: Address = Address::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);

    #[test]
    fn matching_device_gets_the_passkey() {
        assert_eq!(passkey_for(A, A, 1234), Some(1234));
    }

    #[test]
    fn other_device_gets_nothing() {
        assert_eq!(passkey_for(B, A, 1234), None);
    }

    #[test]
    fn passkey_range_is_enforced() {
        assert!(validate_passkey(0).is_ok());
        assert!(validate_passkey(999_999).is_ok());
        assert!(validate_passkey(1_000_000).is_err());
    }
}
