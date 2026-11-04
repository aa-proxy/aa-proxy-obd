// src/cli/pair.rs
//
// Interactive Bluetooth pairing helper. Reads the MAC from the active config,
// registers a passkey agent, attempts to pair, and on success offers to
// persist the passkey into the config via toml_edit (comments preserved).

use crate::adapter::pairing::register_passkey_agent;
use crate::config::{Config, DeviceType};
use anyhow::{anyhow, Context, Result};
use bluer::{Address, Session};
use log::{info, warn};
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
    let _handle = register_passkey_agent(&session, passkey, mac).await?;
    let adapter = session.default_adapter().await.context("default adapter")?;

    // Discover the device if BlueZ doesn't already know it. (adapter.device()
    // always succeeds at building a handle, so check the known-address list.)
    if !adapter.device_addresses().await?.contains(&mac) {
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
            Ok(Err(e)) => return Err(e.context("discovery failed")),
            Err(_) => return Err(anyhow!(
                "discovery timed out after 15s; ensure the device is powered and advertising"
            )),
        }
    }

    let device = adapter.device(mac).context("device handle")?;
    if device.is_paired().await.context("query paired state")? {
        println!("Device {mac} is already paired.");
        info!("existing bond kept; the entered passkey was not used and will not be saved");
        return Ok(());
    }
    info!("attempting pairing");
    device.pair().await.context("pair failed")?;
    println!("Pairing succeeded.");

    print!("Save passkey to {} under [device].{}? [y/N]: ", config_path.display(), key_name);
    io::stdout().flush().ok();
    let mut answer = String::new();
    if let Err(e) = io::stdin().read_line(&mut answer) {
        warn!("passkey not saved: could not read the confirmation: {e}");
        return Ok(());
    }
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
    write_atomic(path, doc.to_string().as_bytes()).context("write config")
}

/// Replace `path` with `contents` through a temporary file in the same
/// directory: the data is synced, the original permissions are applied, and the
/// file is renamed over the original so readers see either the old or the new
/// content. A symlinked path is resolved first, so the link target is replaced
/// and the link itself is kept. Owner and group are carried over where the
/// process is allowed to set them.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let resolved = std::fs::canonicalize(path).context("resolve config path")?;
    let path = resolved.as_path();
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path.file_name().ok_or_else(|| anyhow!("config path has no file name"))?;
    let metadata = std::fs::metadata(path).context("read config metadata")?;
    let permissions = metadata.permissions();
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = dir.join(tmp_name);

    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .context("create temp file")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{fchown, MetadataExt};
            // Changing the owner needs privileges; without them the new file
            // keeps the current user as owner.
            if let Err(e) = fchown(&file, Some(metadata.uid()), Some(metadata.gid())) {
                if e.kind() != std::io::ErrorKind::PermissionDenied {
                    return Err(anyhow::Error::new(e).context("set temp file owner"));
                }
            }
        }
        file.set_permissions(permissions).context("set temp file permissions")?;
        file.write_all(contents).context("write temp file")?;
        file.sync_all().context("sync temp file")?;
        drop(file);
        std::fs::rename(&tmp, path).context("rename temp file over config")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        warn!("config replaced but the directory sync failed: {e}");
    }
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

    #[test]
    fn write_atomic_keeps_mode_and_leaves_no_temp_file() {
        let dir = tempdir_or_panic();
        let path = dir.join("cfg.toml");
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        write_atomic(&path, b"new content").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("cfg.toml")]);
    }

    #[test]
    fn unparseable_config_leaves_original_untouched() {
        let dir = tempdir_or_panic();
        let path = dir.join("cfg.toml");
        let bad = "[device
not toml";
        std::fs::write(&path, bad).unwrap();

        assert!(write_passkey_to_config(&path, "bt_passkey", 1).is_err());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_writes_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let real_dir = tempdir_or_panic();
        let link_dir = tempdir_or_panic();
        let target = real_dir.join("real.toml");
        let link = link_dir.join("link.toml");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_atomic(&link, b"new content").unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new content");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        assert_eq!(std::fs::read_dir(&real_dir).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&link_dir).unwrap().count(), 1);
    }

    /// A scratch directory under the system temp dir, removed when dropped
    /// (also when the test panics).
    struct TempDir(std::path::PathBuf);

    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path { &self.0 }
    }

    impl AsRef<std::path::Path> for TempDir {
        fn as_ref(&self) -> &std::path::Path { &self.0 }
    }

    impl Drop for TempDir {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn tempdir_or_panic() -> TempDir {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("aa-proxy-obd-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
