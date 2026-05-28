// Daemon configuration loaded from /etc/aa-proxy-obd.toml.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Error, ErrorKind};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Bluetooth,
    Usb,
    Wican,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DeviceConfig {
    #[serde(rename = "type")]
    pub kind: DeviceType,

    #[serde(default)]
    pub bt_mac: Option<String>,
    #[serde(default)]
    pub bt_passkey: Option<u32>,

    #[serde(default)]
    pub usb_port: Option<String>,
    #[serde(default)]
    pub usb_baud: Option<u32>,

    #[serde(default)]
    pub wican_mac: Option<String>,
    #[serde(default)]
    pub wican_passkey: Option<u32>,
    #[serde(default)]
    pub wican_max_connect_retries: Option<u8>,
    #[serde(default)]
    pub wican_timeout_secs: Option<u8>,
}

impl Default for DeviceType {
    fn default() -> Self { DeviceType::Bluetooth }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VehicleSection {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub battery_capacity_wh: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonSection {
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: f32,
    #[serde(default = "default_car_sleep_interval_secs")]
    pub car_sleep_interval_secs: f32,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_log_file")]
    pub log_file: String,
    #[serde(default = "default_api_base_url")]
    pub api_base_url: String,
    #[serde(default = "default_bridge_dropouts")]
    pub bridge_dropouts: bool,
    #[serde(default = "default_publish_failure_threshold")]
    pub publish_failure_threshold: u32,
    #[serde(default = "default_publish_breaker_secs")]
    pub publish_breaker_secs: u64,
    #[serde(default = "default_cycle_failure_limit")]
    pub cycle_failure_limit: u32,
}

fn default_poll_interval_secs()        -> f32    { 10.0 }
fn default_car_sleep_interval_secs()   -> f32    { 100.0 }
fn default_log_level()                 -> String { "info".to_string() }
fn default_log_file()                  -> String { "/var/log/aa-proxy-obd.log".to_string() }
fn default_api_base_url()              -> String { "http://localhost".to_string() }
fn default_bridge_dropouts()           -> bool   { true }
fn default_publish_failure_threshold() -> u32    { 5 }
fn default_publish_breaker_secs()      -> u64    { 300 }
fn default_cycle_failure_limit()       -> u32    { 20 }

impl Default for DaemonSection {
    fn default() -> Self {
        Self {
            poll_interval_secs:        default_poll_interval_secs(),
            car_sleep_interval_secs:   default_car_sleep_interval_secs(),
            log_level:                 default_log_level(),
            log_file:                  default_log_file(),
            api_base_url:              default_api_base_url(),
            bridge_dropouts:           default_bridge_dropouts(),
            publish_failure_threshold: default_publish_failure_threshold(),
            publish_breaker_secs:      default_publish_breaker_secs(),
            cycle_failure_limit:       default_cycle_failure_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub device:  DeviceConfig,
    #[serde(default)]
    pub vehicle: VehicleSection,
    #[serde(default)]
    pub daemon:  DaemonSection,
}

impl Config {
    pub fn load(path: &Path) -> io::Result<Self> {
        let raw = fs::read_to_string(path).map_err(|e| {
            Error::new(
                ErrorKind::NotFound,
                format!("Failed to read config '{}': {}", path.display(), e),
            )
        })?;
        let cfg: Config = toml::from_str(&raw).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Failed to parse TOML in '{}': {}", path.display(), e),
            )
        })?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_bluetooth_config() {
        let src = r#"
            [device]
            type = "bluetooth"
            bt_mac = "AA:BB:CC:DD:EE:FF"

            [vehicle]
            profile = "ev6"
        "#;
        let cfg: Config = toml::from_str(src).unwrap();
        assert_eq!(cfg.device.kind, DeviceType::Bluetooth);
        assert_eq!(cfg.device.bt_mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(cfg.vehicle.profile.as_deref(), Some("ev6"));
        assert!((cfg.daemon.poll_interval_secs - 10.0).abs() < f32::EPSILON);
        assert!(cfg.daemon.bridge_dropouts);
    }

    #[test]
    fn parses_full_config_with_overrides() {
        let src = r#"
            [device]
            type = "wican"
            wican_mac = "11:22:33:44:55:66"
            wican_passkey = 123456
            wican_max_connect_retries = 7
            wican_timeout_secs = 15

            [vehicle]
            battery_capacity_wh = 77400

            [daemon]
            poll_interval_secs = 5.0
            car_sleep_interval_secs = 200.0
            log_level = "debug"
            log_file = "/tmp/aa-proxy-obd.log"
            api_base_url = "http://example.test:8080"
            bridge_dropouts = false
            publish_failure_threshold = 3
            publish_breaker_secs = 60
            cycle_failure_limit = 10
        "#;
        let cfg: Config = toml::from_str(src).unwrap();
        assert_eq!(cfg.device.kind, DeviceType::Wican);
        assert_eq!(cfg.device.wican_mac.as_deref(), Some("11:22:33:44:55:66"));
        assert_eq!(cfg.device.wican_passkey, Some(123456));
        assert_eq!(cfg.device.wican_max_connect_retries, Some(7));
        assert_eq!(cfg.device.wican_timeout_secs, Some(15));
        assert_eq!(cfg.vehicle.battery_capacity_wh, Some(77400));
        assert!((cfg.daemon.poll_interval_secs - 5.0).abs() < f32::EPSILON);
        assert!((cfg.daemon.car_sleep_interval_secs - 200.0).abs() < f32::EPSILON);
        assert_eq!(cfg.daemon.log_level, "debug");
        assert_eq!(cfg.daemon.log_file, "/tmp/aa-proxy-obd.log");
        assert_eq!(cfg.daemon.api_base_url, "http://example.test:8080");
        assert!(!cfg.daemon.bridge_dropouts);
        assert_eq!(cfg.daemon.publish_failure_threshold, 3);
        assert_eq!(cfg.daemon.publish_breaker_secs, 60);
        assert_eq!(cfg.daemon.cycle_failure_limit, 10);
    }

    #[test]
    fn rejects_unknown_device_type() {
        let src = r#"
            [device]
            type = "satellite"
        "#;
        let r: Result<Config, _> = toml::from_str(src);
        assert!(r.is_err(), "expected error for unknown device type");
    }
}
