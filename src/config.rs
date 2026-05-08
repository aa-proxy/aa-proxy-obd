use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Error, ErrorKind};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MetricConfig {
    pub name: String,
    pub byte_index: i32,
    pub length: usize,
    pub multiplier: f32,
    pub offset: f32,
    pub signed: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PidConfig {
    pub ecu_tx: String,
    pub ecu_rx: String,
    pub pid: String,
    /// Optional command to send before the PID request (e.g. "10C0" for Zoe extended session).
    /// Errors are silently ignored, matching the original behaviour.
    pub pre_request: Option<String>,
    pub fields: Vec<MetricConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct VehicleConfig {
    pub name: String,
    pub pids: Vec<PidConfig>,
}

impl VehicleConfig {
    pub fn load(car_name: &str) -> io::Result<Self> {
        // Construct the expected profile path: e.g. "/etc/canze-rs/zoe.json"
        let filename = format!("/etc/canze-rs/{}.json", car_name.to_lowercase());
        
        let contents = fs::read_to_string(&filename).map_err(|e| {
            Error::new(
                ErrorKind::NotFound,
                format!("Failed to read profile '{}': {}", filename, e),
            )
        })?;

        let config: VehicleConfig = serde_json::from_str(&contents).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Failed to parse JSON in '{}': {}", filename, e),
            )
        })?;

        Ok(config)
    }
}
