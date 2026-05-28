// src/profile.rs
//
// Vehicle profile loading. Schema migration to sources[] happens in a later
// task; this commit only splits the file out of config.rs so daemon and
// profile configuration live in distinct modules.

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
        let filename = format!("/etc/aa-proxy-obd/{}.json", car_name.to_lowercase());
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
