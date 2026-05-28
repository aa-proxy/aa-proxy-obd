// Vehicle profile schema. Each profile is one JSON file at
// /etc/aa-proxy-obd/<name>.json containing a list of sources; each source is
// either a UdsPid (active request/response) or a Broadcast (passive CAN
// monitor).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Error, ErrorKind};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldSpec {
    pub name: String,

    #[serde(default)]
    pub byte_index: Option<i32>,
    #[serde(default)]
    pub length: Option<usize>,

    #[serde(default)]
    pub bit_offset: Option<u32>,
    #[serde(default)]
    pub bit_length: Option<u32>,

    #[serde(default = "one_f32")]
    pub multiplier: f32,
    #[serde(default)]
    pub offset: f32,
    #[serde(default)]
    pub signed: Option<bool>,
}

fn one_f32() -> f32 { 1.0 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UdsPidSource {
    pub ecu_tx: String,
    pub ecu_rx: String,
    pub pid: String,
    #[serde(default)]
    pub pre_request: Option<String>,
    #[serde(default)]
    pub multiframe: bool,
    pub fields: Vec<FieldSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BroadcastFrame {
    pub can_id: String,
    pub fields: Vec<FieldSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BroadcastSource {
    #[serde(default = "default_broadcast_init")]
    pub init: Vec<String>,
    #[serde(default = "default_broadcast_command")]
    pub command: String,
    #[serde(default = "default_deadline_ms")]
    pub deadline_ms: u64,
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    #[serde(default)]
    pub stop_when: Vec<String>,
    pub frames: Vec<BroadcastFrame>,
}

fn default_broadcast_init() -> Vec<String> {
    vec!["ATCRA".into(), "ATH1".into(), "ATS1".into()]
}
fn default_broadcast_command() -> String { "ATMA".into() }
fn default_deadline_ms()       -> u64    { 6000 }
fn default_idle_timeout_ms()   -> u64    { 600 }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    UdsPid(UdsPidSource),
    Broadcast(BroadcastSource),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Elm327Override {
    #[serde(default)]
    pub init: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VehicleProfile {
    pub name: String,
    #[serde(default)]
    pub elm327: Option<Elm327Override>,
    pub sources: Vec<Source>,
}

impl VehicleProfile {
    pub fn load(car_name: &str) -> io::Result<Self> {
        let filename = format!("/etc/aa-proxy-obd/{}.json", car_name.to_lowercase());
        let raw = fs::read_to_string(&filename).map_err(|e| {
            Error::new(
                ErrorKind::NotFound,
                format!("Failed to read profile '{}': {}", filename, e),
            )
        })?;
        let profile: VehicleProfile = serde_json::from_str(&raw).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Failed to parse JSON in '{}': {}", filename, e),
            )
        })?;
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uds_pid_profile() {
        let raw = r#"{
            "name": "Test EV",
            "sources": [
                {
                    "kind": "uds_pid",
                    "ecu_tx": "7E4",
                    "ecu_rx": "7EC",
                    "pid": "220105",
                    "fields": [
                        {"name": "battery_level_percentage",
                         "byte_index": 31, "length": 1,
                         "multiplier": 0.5, "offset": 0.0}
                    ]
                }
            ]
        }"#;
        let p: VehicleProfile = serde_json::from_str(raw).unwrap();
        assert_eq!(p.name, "Test EV");
        assert_eq!(p.sources.len(), 1);
        match &p.sources[0] {
            Source::UdsPid(s) => {
                assert_eq!(s.pid, "220105");
                assert_eq!(s.fields.len(), 1);
                assert!(!s.multiframe);
            }
            _ => panic!("expected UdsPid"),
        }
    }

    #[test]
    fn parses_broadcast_profile() {
        let raw = r#"{
            "name": "Test BC",
            "sources": [
                {
                    "kind": "broadcast",
                    "frames": [
                        {"can_id": "673", "fields": [
                            {"name": "tire_fl_kpa",
                             "byte_index": 5, "length": 1,
                             "multiplier": 1.3725}
                        ]}
                    ]
                }
            ]
        }"#;
        let p: VehicleProfile = serde_json::from_str(raw).unwrap();
        match &p.sources[0] {
            Source::Broadcast(b) => {
                assert_eq!(b.command, "ATMA");
                assert_eq!(b.frames.len(), 1);
                assert_eq!(b.frames[0].can_id, "673");
            }
            _ => panic!("expected Broadcast"),
        }
    }

    #[test]
    fn rejects_profile_without_sources() {
        let raw = r#"{"name": "Broken"}"#;
        let r: Result<VehicleProfile, _> = serde_json::from_str(raw);
        assert!(r.is_err());
    }

    #[test]
    fn all_shipped_profiles_deserialize() {
        for name in ["ev6", "ioniq5", "mg4", "niro", "zoe"] {
            let path = format!(
                "{}/profiles/{}.json",
                env!("CARGO_MANIFEST_DIR"),
                name
            );
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {}", path, e));
            let p: VehicleProfile = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("parse {}: {}", path, e));
            assert!(!p.sources.is_empty(), "{} has no sources", name);
        }
    }
}
