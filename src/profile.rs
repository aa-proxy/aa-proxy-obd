// Vehicle profile schema. Each profile is one JSON file at
// /etc/aa-proxy-obd/<name>.json containing a list of sources; each source is
// either a UdsPid (active request/response) or a Broadcast (passive CAN
// monitor).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Error, ErrorKind};
use std::path::Path;

const PROFILE_DIR: &str = "/etc/aa-proxy-obd";

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
    /// A disabled field stays in the file (and is still validated) but is not
    /// extracted or reported.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn one_f32() -> f32 { 1.0 }
fn default_true() -> bool { true }

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

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// CAN identifiers take 3, 6 or 8 hex digits.
pub fn is_valid_can_id(s: &str) -> bool {
    is_hex(s) && matches!(s.len(), 3 | 6 | 8)
}

fn check_field(ctx: &str, f: &FieldSpec, max_len: usize) -> Result<(), String> {
    let ctx = format!("{ctx}, field '{}'", f.name);
    let any_bytes = f.byte_index.is_some() || f.length.is_some();
    let any_bits = f.bit_offset.is_some() || f.bit_length.is_some();
    if !any_bytes && !any_bits {
        return Err(format!("{ctx}: set byte_index+length or bit_offset+bit_length"));
    }
    if any_bytes && (f.byte_index.is_none() || f.length.is_none()) {
        return Err(format!("{ctx}: byte_index and length must be set together"));
    }
    if any_bits && (f.bit_offset.is_none() || f.bit_length.is_none()) {
        return Err(format!("{ctx}: bit_offset and bit_length must be set together"));
    }
    if let Some(n) = f.length {
        if !(1..=max_len).contains(&n) {
            return Err(format!("{ctx}: length must be 1..={max_len}, got {n}"));
        }
    }
    if let Some(n) = f.bit_length {
        if !(1..=16).contains(&n) {
            return Err(format!("{ctx}: bit_length must be 1..=16, got {n}"));
        }
    }
    Ok(())
}

impl VehicleProfile {
    /// Checks the structural rules the decoders rely on. `file` names the
    /// profile in error messages.
    pub fn validate(&self, file: &str) -> Result<(), String> {
        for (i, source) in self.sources.iter().enumerate() {
            match source {
                Source::UdsPid(s) => {
                    let ctx = format!("{file}: source {i} (uds_pid)");
                    for (key, v) in [("ecu_tx", &s.ecu_tx), ("ecu_rx", &s.ecu_rx)] {
                        if !is_valid_can_id(v) {
                            return Err(format!(
                                "{ctx}: {key} '{v}' must be 3, 6 or 8 hex digits"
                            ));
                        }
                    }
                    if !is_hex(&s.pid) || s.pid.len() % 2 != 0 {
                        return Err(format!(
                            "{ctx}: pid '{}' must be hex with an even number of digits",
                            s.pid
                        ));
                    }
                    for f in &s.fields {
                        check_field(&ctx, f, 4)?;
                    }
                }
                Source::Broadcast(b) => {
                    let ctx = format!("{file}: source {i} (broadcast)");
                    if b.deadline_ms == 0 {
                        return Err(format!("{ctx}: deadline_ms must be > 0"));
                    }
                    for frame in &b.frames {
                        let id = &frame.can_id;
                        if !is_hex(id) || !matches!(id.len(), 3 | 8) {
                            return Err(format!(
                                "{ctx}: can_id '{id}' must be 3 or 8 hex digits"
                            ));
                        }
                        let fctx = format!("{ctx}, frame {id}");
                        for f in &frame.fields {
                            check_field(&fctx, f, 2)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn load(car_name: &str) -> io::Result<Self> {
        Self::load_from(Path::new(PROFILE_DIR), car_name)
    }

    fn load_from(dir: &Path, car_name: &str) -> io::Result<Self> {
        let name = car_name.to_lowercase();
        if name.is_empty()
            || name.contains("..")
            || name.contains(['/', '\\'])
            || !name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Invalid profile name '{car_name}': use only a-z, 0-9, '_', '-' and '.', with no '..' or path separator"),
            ));
        }
        let path = dir.join(format!("{name}.json"));
        let filename = path.display().to_string();
        let raw = fs::read_to_string(&path).map_err(|e| {
            Error::new(e.kind(), format!("Failed to read profile '{}': {}", filename, e))
        })?;
        let mut profile: VehicleProfile = serde_json::from_str(&raw).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Failed to parse JSON in '{}': {}", filename, e),
            )
        })?;
        profile
            .validate(&filename)
            .map_err(|msg| Error::new(ErrorKind::InvalidData, format!("Invalid profile {msg}")))?;
        // ATMA prints CAN-IDs in uppercase; frames are matched against them as text.
        for source in &mut profile.sources {
            if let Source::Broadcast(b) = source {
                for frame in &mut b.frames {
                    frame.can_id.make_ascii_uppercase();
                }
            }
        }
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
        let dir = format!("{}/profiles", env!("CARGO_MANIFEST_DIR"));
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
            let p: VehicleProfile = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e));
            assert!(!p.sources.is_empty(), "{} has no sources", path.display());
            p.validate(&path.display().to_string())
                .unwrap_or_else(|e| panic!("validate: {e}"));
            seen += 1;
        }
        assert!(seen >= 6, "expected the shipped profiles, found {seen}");
    }

    fn with_source(json_fields: &str, head: &str) -> Result<(), String> {
        let raw = format!(
            r#"{{"name":"T","sources":[{{"kind":"uds_pid",{head},"fields":[{json_fields}]}}]}}"#
        );
        let p: VehicleProfile = serde_json::from_str(&raw).unwrap();
        p.validate("t.json")
    }

    const OK_HEAD: &str = r#""ecu_tx":"7E4","ecu_rx":"7EC","pid":"220105""#;

    #[test]
    fn ecu_ids_and_pid_are_checked() {
        for (head, needle) in [
            (r#""ecu_tx":"7E","ecu_rx":"7EC","pid":"220105""#, "ecu_tx"),
            (r#""ecu_tx":"7E4","ecu_rx":"7EC1","pid":"220105""#, "ecu_rx"),
            (r#""ecu_tx":"7G4","ecu_rx":"7EC","pid":"220105""#, "ecu_tx"),
            (r#""ecu_tx":"7E4","ecu_rx":"7EC","pid":"22010""#, "pid"),
            (r#""ecu_tx":"7E4","ecu_rx":"7EC","pid":"22ZZ""#, "pid"),
        ] {
            let e = with_source(r#"{"name":"f","byte_index":3,"length":1}"#, head).unwrap_err();
            assert!(e.contains(needle) && e.contains("t.json") && e.contains("source 0"), "{e}");
        }
        for tx in ["7E4", "18DA10F1", "18DA10"] {
            let head = format!(r#""ecu_tx":"{tx}","ecu_rx":"7EC","pid":"220105""#);
            with_source(r#"{"name":"f","byte_index":3,"length":1}"#, &head).unwrap();
        }
    }

    #[test]
    fn field_addressing_needs_a_complete_pair() {
        let cases = [
            (r#"{"name":"f"}"#, "byte_index+length or bit_offset"),
                        (r#"{"name":"f","byte_index":1}"#, "together"),
            (r#"{"name":"f","bit_offset":1}"#, "together"),
            (r#"{"name":"f","byte_index":1,"length":0}"#, "length must be 1..=4"),
            (r#"{"name":"f","byte_index":1,"length":5}"#, "length must be 1..=4"),
            (r#"{"name":"f","bit_offset":1,"bit_length":0}"#, "bit_length must be 1..=16"),
            (r#"{"name":"f","bit_offset":1,"bit_length":17}"#, "bit_length must be 1..=16"),
        ];
        for (field, needle) in cases {
            let e = with_source(field, OK_HEAD).unwrap_err();
            assert!(e.contains(needle) && e.contains("field 'f'"), "{field}: {e}");
        }
        with_source(r#"{"name":"f","bit_offset":4,"bit_length":16}"#, OK_HEAD).unwrap();
        let both = r#"{"name":"f","byte_index":1,"length":1,"bit_offset":0,"bit_length":1}"#;
        with_source(both, OK_HEAD).unwrap();
        with_source(r#"{"name":"f","byte_index":-2,"length":3}"#, OK_HEAD).unwrap();
        with_source(r#"{"name":"f","byte_index":-4,"length":4}"#, OK_HEAD).unwrap();
    }

    fn broadcast(deadline: u64, idle: u64, can_id: &str) -> Result<(), String> {
        let raw = format!(
            r#"{{"name":"T","sources":[{{"kind":"broadcast","deadline_ms":{deadline},"idle_timeout_ms":{idle},
            "frames":[{{"can_id":"{can_id}","fields":[{{"name":"f","byte_index":0,"length":1}}]}}]}}]}}"#
        );
        let p: VehicleProfile = serde_json::from_str(&raw).unwrap();
        p.validate("bc.json")
    }

    #[test]
    fn broadcast_field_length_is_one_or_two() {
        let with_len = |n: usize| {
            let raw = format!(
                r#"{{"name":"T","sources":[{{"kind":"broadcast","frames":[{{"can_id":"673","fields":[{{"name":"f","byte_index":0,"length":{n}}}]}}]}}]}}"#
            );
            let p: VehicleProfile = serde_json::from_str(&raw).unwrap();
            p.validate("bc.json")
        };
        with_len(1).unwrap();
        with_len(2).unwrap();
        let e = with_len(3).unwrap_err();
        assert!(e.contains("length must be 1..=2") && e.contains("field 'f'"), "{e}");
    }

    #[test]
    fn broadcast_timing_and_ids_are_checked() {
        broadcast(6000, 600, "673").unwrap();
        broadcast(600, 600, "18DAF110").unwrap();
        assert!(broadcast(0, 0, "673").unwrap_err().contains("deadline_ms"));
        broadcast(500, 600, "673").unwrap();
        assert!(broadcast(6000, 600, "67").unwrap_err().contains("can_id"));
        assert!(broadcast(6000, 600, "67G").unwrap_err().contains("can_id"));
        assert!(broadcast(6000, 600, "18DAF1").unwrap_err().contains("can_id"));
    }

    #[test]
    fn car_name_charset_is_enforced() {
        for bad in ["../ev6", "ev6/x", "ev6\\x", "..", "a..b", "ev 6", "", "é"] {
            let e = VehicleProfile::load_from(Path::new("/nonexistent"), bad).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidInput, "{bad:?}");
        }
        // A valid name reaches the filesystem and keeps the NotFound kind.
        let e = VehicleProfile::load_from(Path::new("/nonexistent"), "My_Car-2.v1").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NotFound);
        assert!(e.to_string().contains("my_car-2.v1.json"), "{e}");
    }

    #[test]
    fn load_validates_and_names_the_file() {
        let dir = format!("{}/profiles", env!("CARGO_MANIFEST_DIR"));
        let p = VehicleProfile::load_from(Path::new(&dir), "EV6").unwrap();
        assert!(!p.sources.is_empty());

        let tmp = std::env::temp_dir().join(format!("aa-proxy-obd-prof-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("bad.json"),
            r#"{"name":"B","sources":[{"kind":"uds_pid","ecu_tx":"7E","ecu_rx":"7EC","pid":"2201","fields":[]}]}"#,
        )
        .unwrap();
        let e = VehicleProfile::load_from(&tmp, "bad").unwrap_err();
        std::fs::remove_dir_all(&tmp).ok();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert!(e.to_string().contains("bad.json") && e.to_string().contains("ecu_tx"), "{e}");
    }

    #[test]
    fn load_uppercases_broadcast_can_ids() {
        let tmp = std::env::temp_dir().join(format!("aa-proxy-obd-prof-case-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("lower.json"),
            r#"{"name":"L","sources":[{"kind":"broadcast","frames":[
                {"can_id":"6a3","fields":[{"name":"x","byte_index":0,"length":1}]},
                {"can_id":"18daf110","fields":[{"name":"y","byte_index":0,"length":1}]}]}]}"#,
        )
        .unwrap();
        let p = VehicleProfile::load_from(&tmp, "lower");
        std::fs::remove_dir_all(&tmp).ok();
        let Source::Broadcast(b) = &p.unwrap().sources[0] else { panic!("expected Broadcast") };
        let ids: Vec<&str> = b.frames.iter().map(|f| f.can_id.as_str()).collect();
        assert_eq!(ids, ["6A3", "18DAF110"]);
    }
}
