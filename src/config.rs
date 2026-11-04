// Daemon configuration loaded from /etc/aa-proxy-obd.toml.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Error, ErrorKind};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum DeviceType {
    #[default]
    Bluetooth,
    Usb,
    Wican,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DeviceConfig {
    #[serde(rename = "type")]
    pub kind: DeviceType,

    // Bluetooth options, shared by the "bluetooth" and "wican" device types
    // (both connect over Bluetooth).
    #[serde(default)]
    pub bt_mac: Option<String>,
    #[serde(default)]
    pub bt_passkey: Option<u32>,
    #[serde(default)]
    pub bt_max_connect_retries: Option<u8>,
    #[serde(default)]
    pub bt_timeout_secs: Option<u8>,

    #[serde(default)]
    pub usb_port: Option<String>,
    #[serde(default)]
    pub usb_baud: Option<u32>,
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
    /// Topic for POST /raw-topic-data. When set, each poll cycle sends all of its
    /// metrics there as one JSON object; when unset, nothing is sent.
    #[serde(default)]
    pub raw_topic: Option<String>,
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
            raw_topic:                 None,
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

const PLACEHOLDER_MAC: &str = "AA:BB:CC:DD:EE:FF";
const MIN_INTERVAL_SECS: f32 = 0.1;
const MAX_RAW_TOPIC_CHARS: usize = 64;

const DEVICE_KEYS: &[&str] = &[
    "type", "bt_mac", "bt_passkey", "bt_max_connect_retries", "bt_timeout_secs",
    "usb_port", "usb_baud",
];
const VEHICLE_KEYS: &[&str] = &["profile", "battery_capacity_wh"];
const DAEMON_KEYS: &[&str] = &[
    "poll_interval_secs", "car_sleep_interval_secs", "log_level", "log_file",
    "api_base_url", "bridge_dropouts", "publish_failure_threshold",
    "publish_breaker_secs", "cycle_failure_limit", "raw_topic",
];

fn invalid(msg: String) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

fn check_interval(key: &str, value: f32) -> io::Result<()> {
    if !value.is_finite() || value < MIN_INTERVAL_SECS {
        return Err(invalid(format!(
            "daemon.{key} must be a finite number >= {MIN_INTERVAL_SECS}, got {value}"
        )));
    }
    if std::time::Duration::try_from_secs_f32(value).is_err() {
        return Err(invalid(format!("daemon.{key} is too large: {value}")));
    }
    Ok(())
}

/// Paths ("section.key", or just "section") of every key that no section
/// defines.
fn unknown_keys(value: &toml::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(root) = value.as_table() else { return out };
    for (section, body) in root {
        let known: &[&str] = match section.as_str() {
            "device" => DEVICE_KEYS,
            "vehicle" => VEHICLE_KEYS,
            "daemon" => DAEMON_KEYS,
            _ => {
                out.push(section.clone());
                continue;
            }
        };
        if let Some(table) = body.as_table() {
            for key in table.keys() {
                if !known.contains(&key.as_str()) {
                    out.push(format!("{section}.{key}"));
                }
            }
        }
    }
    out
}

impl Config {
    /// Reads, parses and validates the file. Unknown-key warnings are logged,
    /// so the logger must already be installed.
    pub fn load(path: &Path) -> io::Result<Self> {
        let (cfg, warnings) = Self::load_with_warnings(path)?;
        for w in warnings {
            log::warn!("{w}");
        }
        Ok(cfg)
    }

    /// Like [`Config::load`], but returns the unknown-key warnings instead of
    /// logging them, for callers that install the logger from the config.
    pub fn load_with_warnings(path: &Path) -> io::Result<(Self, Vec<String>)> {
        let raw = fs::read_to_string(path).map_err(|e| {
            Error::new(
                e.kind(),
                format!("Failed to read config '{}': {}", path.display(), e),
            )
        })?;
        Self::parse(&raw, path)
    }

    fn parse(raw: &str, path: &Path) -> io::Result<(Self, Vec<String>)> {
        let cfg: Config = toml::from_str(raw).map_err(|e| {
            invalid(format!("Failed to parse TOML in '{}': {}", path.display(), e))
        })?;
        cfg.validate().map_err(|e| {
            Error::new(e.kind(), format!("Invalid config '{}': {}", path.display(), e))
        })?;
        let warnings = match toml::from_str::<toml::Value>(raw) {
            Ok(v) => unknown_keys(&v)
                .into_iter()
                .map(|k| format!("Unknown config key '{k}' in '{}' (ignored)", path.display()))
                .collect(),
            Err(_) => Vec::new(),
        };
        Ok((cfg, warnings))
    }

    pub fn validate(&self) -> io::Result<()> {
        let d = &self.daemon;
        check_interval("poll_interval_secs", d.poll_interval_secs)?;
        check_interval("car_sleep_interval_secs", d.car_sleep_interval_secs)?;
        if let Some(topic) = &d.raw_topic {
            let ok = !topic.is_empty()
                && topic.len() <= MAX_RAW_TOPIC_CHARS
                && topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !ok {
                return Err(invalid(format!(
                    "daemon.raw_topic must be 1 to {MAX_RAW_TOPIC_CHARS} characters of A-Z, a-z, 0-9, '_' or '-', got '{topic}'"
                )));
            }
        }
        if self.device.usb_baud == Some(0) {
            return Err(invalid("device.usb_baud must be >= 1".into()));
        }
        if let Some(mac) = &self.device.bt_mac {
            if mac.trim().eq_ignore_ascii_case(PLACEHOLDER_MAC) {
                return Err(invalid(format!(
                    "device.bt_mac is the placeholder {PLACEHOLDER_MAC}; set it to your adapter's MAC address"
                )));
            }
        }
        Ok(())
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
            bt_mac = "11:22:33:44:55:66"
            bt_passkey = 123456
            bt_max_connect_retries = 7
            bt_timeout_secs = 15

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
        assert_eq!(cfg.device.bt_mac.as_deref(), Some("11:22:33:44:55:66"));
        assert_eq!(cfg.device.bt_passkey, Some(123456));
        assert_eq!(cfg.device.bt_max_connect_retries, Some(7));
        assert_eq!(cfg.device.bt_timeout_secs, Some(15));
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

    fn parse(src: &str) -> io::Result<(Config, Vec<String>)> {
        Config::parse(src, Path::new("test.toml"))
    }

    fn with_daemon(lines: &str) -> String {
        format!("[device]\ntype = \"bluetooth\"\n[daemon]\n{lines}\n")
    }

    #[test]
    fn rejects_bad_intervals_naming_the_key() {
        for bad in ["-1.0", "0", "0.05", "nan", "inf", "1e30"] {
            for key in ["poll_interval_secs", "car_sleep_interval_secs"] {
                let e = parse(&with_daemon(&format!("{key} = {bad}"))).unwrap_err();
                assert_eq!(e.kind(), ErrorKind::InvalidData, "{key}={bad}");
                assert!(e.to_string().contains(key), "{key}={bad}: {e}");
            }
        }
        assert!(parse(&with_daemon("poll_interval_secs = 0.1")).is_ok());
    }

    #[test]
    fn zero_threshold_is_accepted_and_zero_baud_rejected() {
        assert!(parse(&with_daemon("publish_failure_threshold = 0")).is_ok());
        let e = parse("[device]\ntype = \"usb\"\nusb_baud = 0\n").unwrap_err();
        assert!(e.to_string().contains("usb_baud"));
        // Zero disables the watchdog and the breaker timer; both stay valid.
        assert!(parse(&with_daemon("cycle_failure_limit = 0\npublish_breaker_secs = 0")).is_ok());
    }

    #[test]
    fn out_of_range_passkey_still_loads() {
        // The range is enforced when the agent registers, so `pair` can load
        // a config holding a bad value and replace it.
        let bad = "[device]\ntype = \"bluetooth\"\nbt_passkey = 1000000\n";
        let cfg = parse(bad).unwrap();
        assert_eq!(cfg.0.device.bt_passkey, Some(1_000_000));
    }

    #[test]
    fn placeholder_mac_is_rejected_case_insensitively() {
        for mac in ["AA:BB:CC:DD:EE:FF", "aa:bb:cc:dd:ee:ff"] {
            let src = format!("[device]\ntype = \"bluetooth\"\nbt_mac = \"{mac}\"\n");
            let e = parse(&src).unwrap_err();
            assert!(e.to_string().contains("bt_mac"), "{e}");
            assert!(e.to_string().contains("adapter's MAC"), "{e}");
        }
        let src = "[device]\ntype = \"bluetooth\"\nbt_mac = \"11:22:33:44:55:66\"\n";
        assert!(parse(src).is_ok());
    }

    #[test]
    fn unknown_keys_are_reported_but_load() {
        let src = r#"
            [device]
            type = "bluetooth"
            bt_mak = "11:22:33:44:55:66"
            [daemon]
            poll_intervall_secs = 5
            [extra]
            x = 1
        "#;
        let (cfg, warnings) = parse(src).unwrap();
        assert_eq!(cfg.device.kind, DeviceType::Bluetooth);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        let all = warnings.join("\n");
        for key in ["device.bt_mak", "daemon.poll_intervall_secs", "'extra'"] {
            assert!(all.contains(key), "missing {key} in {all}");
        }
    }

    #[test]
    fn read_errors_keep_their_kind() {
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("does-not-exist.toml");
        let e = Config::load_with_warnings(&missing).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NotFound);

        // Reading a directory fails with a kind other than NotFound.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let e = Config::load_with_warnings(dir).unwrap_err();
        assert_ne!(e.kind(), ErrorKind::NotFound, "{e}");
        assert_eq!(e.kind(), fs::read_to_string(dir).unwrap_err().kind());
    }

    #[test]
    fn known_key_lists_match_the_struct_fields() {
        let cfg = Config {
            device: DeviceConfig {
                kind: DeviceType::Usb,
                bt_mac: Some("11:22:33:44:55:66".into()),
                bt_passkey: Some(1),
                bt_max_connect_retries: Some(1),
                bt_timeout_secs: Some(1),
                usb_port: Some("p".into()),
                usb_baud: Some(1),
            },
            vehicle: VehicleSection { profile: Some("p".into()), battery_capacity_wh: Some(1) },
            daemon: DaemonSection { raw_topic: Some("obd".into()), ..DaemonSection::default() },
        };
        let value = toml::Value::try_from(&cfg).unwrap();
        for (section, known) in [("device", DEVICE_KEYS), ("vehicle", VEHICLE_KEYS), ("daemon", DAEMON_KEYS)] {
            let mut got: Vec<&str> =
                value[section].as_table().unwrap().keys().map(String::as_str).collect();
            let mut want = known.to_vec();
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want, "section {section}");
        }
        assert_eq!(value.as_table().unwrap().len(), 3);
        assert!(unknown_keys(&value).is_empty());
    }

    #[test]
    fn raw_topic_accepts_a_plain_name_only() {
        let with = |topic: &str| format!(
            "[device]\ntype = \"usb\"\nusb_port = \"/dev/ttyUSB0\"\n[daemon]\nraw_topic = \"{topic}\"\n"
        );
        let (cfg, _) = Config::parse(&with("obd_golf-7"), Path::new("t")).unwrap();
        assert_eq!(cfg.daemon.raw_topic.as_deref(), Some("obd_golf-7"));
        for bad in ["", "obd.rpm", "has space", &"x".repeat(65)] {
            let e = Config::parse(&with(bad), Path::new("t")).unwrap_err();
            assert!(e.to_string().contains("daemon.raw_topic"), "{bad}: {e}");
        }
    }

    #[test]
    fn shipped_config_loads_without_bt_mac() {
        // bt_mac is optional at load; adapter setup in main requires it for
        // the bluetooth and wican types.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("configs/aa-proxy-obd.toml");
        let (cfg, warnings) = Config::load_with_warnings(&path).unwrap();
        assert!(cfg.device.bt_mac.is_none());
        assert!(warnings.is_empty(), "{warnings:?}");
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
