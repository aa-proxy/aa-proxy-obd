// src/cli/pair.rs
//
// Interactive Bluetooth pairing helper. Reads the MAC from the active config,
// registers a passkey agent, attempts to pair, and on success offers to
// persist the passkey into the config via toml_edit (comments preserved).

use crate::adapter::pairing::register_passkey_agent;
use crate::config::{Config, DeviceType};
use anyhow::{anyhow, Context, Result};
use bluer::{Address, Session};
use log::info;
use std::io::{self, Write};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use tokio::time::timeout;

pub async fn run(config_path: &Path, passkey_arg: Option<u32>) -> Result<()> {
    let cfg = Config::load(config_path).context("load config")?;

    let (mac_str, key_name) = match cfg.device.kind {
        DeviceType::Bluetooth | DeviceType::Wican => (
            cfg.device.bt_mac.clone().ok_or_else(|| anyhow!("[device].bt_mac is required"))?,
            "bt_passkey",
        ),
        DeviceType::Usb => return Err(anyhow!("`pair` is not applicable for device.type = 'usb'")),
    };
    let mac = Address::from_str(&mac_str).map_err(|_| anyhow!("invalid MAC '{mac_str}'"))?;

    let passkey: u32 = match passkey_arg {
        Some(p) => p,
        None => {
            print!("Enter passkey for {mac}: ");
            io::stdout().flush().ok();
            let mut line = String::new();
            io::stdin().read_line(&mut line).context("read passkey")?;
            line.trim().parse::<u32>().context("passkey must be a number")?
        }
    };

    let session = Session::new().await.context("bluer Session::new")?;
    let _handle = register_passkey_agent(&session, passkey).await?;
    let adapter = session.default_adapter().await.context("default adapter")?;

    // Discover the device if BlueZ doesn't already know it.
    if adapter.device(mac).is_err() {
        info!("device not known; running discovery for up to 15s");
        adapter.set_discovery_filter(Default::default()).await.ok();
        let mut events = adapter.discover_devices().await?;
        let found = timeout(Duration::from_secs(15), async {
            use futures_util::StreamExt;
            while let Some(ev) = events.next().await {
                if let bluer::AdapterEvent::DeviceAdded(addr) = ev {
                    if addr == mac { return Ok::<(), anyhow::Error>(()); }
                }
            }
            Err(anyhow!("discovery ended without seeing the target"))
        }).await;
        match found {
            Ok(Ok(())) => info!("device discovered"),
            _ => return Err(anyhow!("discovery timed out; ensure the device is powered and advertising")),
        }
    }

    let device = adapter.device(mac).context("device handle")?;
    if device.is_paired().await.unwrap_or(false) {
        println!("Device {mac} is already paired.");
    } else {
        info!("attempting pairing");
        device.pair().await.context("pair failed")?;
        println!("Pairing succeeded.");
    }

    print!("Save passkey to {} under [device].{}? [y/N]: ", config_path.display(), key_name);
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).ok();
    if matches!(answer.trim(), "y" | "Y" | "yes") {
        write_passkey_to_config(config_path, key_name, passkey)?;
        println!("Saved.");
    } else {
        println!("Not saved. (You can still set [device].{key_name} manually.)");
    }
    Ok(())
}

/// Update the config file in place, preserving comments and formatting.
fn write_passkey_to_config(path: &Path, key_name: &str, passkey: u32) -> Result<()> {
    let raw = std::fs::read_to_string(path).context("read config for write-back")?;
    let mut doc = raw.parse::<toml_edit::DocumentMut>().context("parse config as TOML")?;
    if doc.get("device").is_none() {
        doc["device"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["device"][key_name] = toml_edit::value(passkey as i64);
    std::fs::write(path, doc.to_string()).context("write config")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_passkey_preserves_comments_and_formatting() {
        let dir = tempdir_or_panic();
        let path = dir.join("aa-proxy-obd.toml");
        std::fs::write(&path, "# leading comment\n[device]\ntype   = \"bluetooth\"   # device type\nbt_mac = \"AA:BB:CC:DD:EE:FF\"\n").unwrap();

        write_passkey_to_config(&path, "bt_passkey", 1234).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# leading comment"), "leading comment lost");
        assert!(after.contains("# device type"), "inline comment lost");
        assert!(after.contains("bt_passkey = 1234"), "key not written");
    }

    fn tempdir_or_panic() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("aa-proxy-obd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
