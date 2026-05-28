extern crate ctrlc;
use clap::Parser;
use log::{error, info};
use simplelog::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod adapter;
mod config;
mod profile;

use crate::adapter::{Adapter, AdapterError};
use crate::adapter::bluetooth::BluetoothElm327Adapter;
use crate::config::{Config, DeviceType};
use crate::profile::VehicleProfile;

use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize, Default, Clone)]
struct BatteryData {
    #[serde(skip_serializing_if = "Option::is_none")] battery_level_percentage: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] external_temp_celsius: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] battery_capacity_wh: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] battery_level_wh: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")] battery_state_of_health: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] battery_voltage: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] battery_current: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] interior_temp_celsius: Option<f32>,
}
#[derive(Debug, Serialize, Default, Clone)]
struct OdometerData { odometer_km: f32, #[serde(skip_serializing_if = "Option::is_none")] trip_km: Option<f32> }
#[derive(Debug, Serialize, Default, Clone)]
struct TirePressureData { pressures_kpa: Vec<f32> }

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)] debug: bool,
    #[arg(short, long, default_value = "/etc/aa-proxy-obd.toml")]
    config: PathBuf,
}

fn logging_init(debug: bool) {
    let conf = ConfigBuilder::new()
        .set_time_format("%F, %H:%M:%S%.3f".to_string())
        .build();
    let level = if debug { LevelFilter::Debug } else { LevelFilter::Info };
    CombinedLogger::init(vec![
        TermLogger::new(level, conf, TerminalMode::Mixed, ColorChoice::Auto),
    ]).expect("logger init");
}

const POLL_INTERVAL_SECS: f32 = 10.0;
const CAR_SLEEP_INTERVAL_SECS: f32 = 100.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    logging_init(args.debug);
    info!("aa-proxy-obd started");

    let cfg = Config::load(&args.config)
        .map_err(|e| { error!("config: {e}"); e })?;

    if cfg.device.kind != DeviceType::Bluetooth {
        error!("device.type must be 'bluetooth' in this build (usb/wican land in later commits)");
        return Ok(());
    }
    let mac = cfg.device.bt_mac.clone()
        .ok_or_else(|| anyhow::anyhow!("device.bt_mac is required for type='bluetooth'"))?;
    let car_model = cfg.vehicle.profile.clone()
        .ok_or_else(|| anyhow::anyhow!("vehicle.profile is required"))?;
    let battery_capacity_wh = cfg.vehicle.battery_capacity_wh;

    let profile = VehicleProfile::load(&car_model)?;
    info!("Profile loaded: {}", profile.name);

    let mut adapter = BluetoothElm327Adapter::new(&mac, profile)?;

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
        .expect("Ctrl-C handler");

    let mut poll_deadline = Instant::now();
    let client = Client::new();

    'connect: loop {
        if !running.load(Ordering::SeqCst) { info!("shutdown"); break; }

        if let Err(e) = adapter.connect().await {
            info!("connect failed ({e}); retrying in 10s");
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue 'connect;
        }

        loop {
            if !running.load(Ordering::SeqCst) { continue 'connect; }
            if Instant::now() < poll_deadline {
                tokio::time::sleep(Duration::from_millis(30)).await;
                continue;
            }
            match adapter.poll().await {
                Ok(metrics) => {
                    publish(&client, &metrics, battery_capacity_wh).await;
                    poll_deadline = Instant::now() + Duration::from_secs_f32(POLL_INTERVAL_SECS);
                }
                Err(AdapterError::Transient(e)) => {
                    info!("transient: {e:#}");
                    poll_deadline = Instant::now() + Duration::from_secs_f32(POLL_INTERVAL_SECS);
                }
                Err(AdapterError::FatalConn(e)) => {
                    info!("connection lost: {e:#}");
                    continue 'connect;
                }
                Err(AdapterError::Sleeping) => {
                    info!("car asleep; long-poll");
                    poll_deadline = Instant::now() + Duration::from_secs_f32(CAR_SLEEP_INTERVAL_SECS);
                }
                Err(AdapterError::Permanent(e)) => {
                    error!("permanent: {e:#}");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

async fn publish(client: &Client, m: &HashMap<String, f32>, capacity: Option<u32>) {
    let battery_present = ["battery_level_percentage","external_temp_celsius","battery_level_wh",
        "battery_state_of_health","battery_voltage","battery_current"].iter().any(|k| m.contains_key(*k));
    if battery_present {
        let data = BatteryData {
            battery_level_percentage: m.get("battery_level_percentage").copied(),
            external_temp_celsius: m.get("external_temp_celsius").copied(),
            battery_capacity_wh: capacity,
            battery_level_wh: m.get("battery_level_wh").map(|&v| v as u64),
            battery_state_of_health: m.get("battery_state_of_health").copied(),
            battery_voltage: m.get("battery_voltage").copied(),
            battery_current: m.get("battery_current").copied(),
            interior_temp_celsius: m.get("interior_temp_celsius").copied(),
        };
        if let Err(e) = client.post("http://localhost/battery").json(&data).send().await {
            log::warn!("POST /battery: {e}");
        }
    }
    if let Some(&odo) = m.get("odometer_km") {
        let data = OdometerData { odometer_km: odo, trip_km: None };
        let _ = client.post("http://localhost/odometer").json(&data).send().await;
    }
    if m.contains_key("tire_fl_kpa") {
        let data = TirePressureData {
            pressures_kpa: vec![
                m.get("tire_fl_kpa").copied().unwrap_or(0.0),
                m.get("tire_fr_kpa").copied().unwrap_or(0.0),
                m.get("tire_rl_kpa").copied().unwrap_or(0.0),
                m.get("tire_rr_kpa").copied().unwrap_or(0.0),
            ],
        };
        let _ = client.post("http://localhost/tire-pressure").json(&data).send().await;
    }
}
