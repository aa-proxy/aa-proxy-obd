extern crate ctrlc;
use clap::Parser;
use log::{error, info};
use simplelog::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod adapter;
mod config;
mod output;
mod profile;
mod scheduler;

use crate::adapter::bluetooth::BluetoothElm327Adapter;
use crate::config::{Config, DeviceType};
use crate::output::Publisher;
use crate::profile::VehicleProfile;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)] debug: bool,
    #[arg(short, long, default_value = "/etc/aa-proxy-obd.toml")] config: PathBuf,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    logging_init(args.debug);
    info!("aa-proxy-obd started");

    let cfg = Config::load(&args.config)
        .map_err(|e| { error!("config: {e}"); e })?;

    if cfg.device.kind != DeviceType::Bluetooth {
        error!("device.type must be 'bluetooth' in this build");
        return Ok(());
    }
    let mac = cfg.device.bt_mac.clone()
        .ok_or_else(|| anyhow::anyhow!("device.bt_mac required"))?;
    let car_model = cfg.vehicle.profile.clone()
        .ok_or_else(|| anyhow::anyhow!("vehicle.profile required"))?;

    let profile = VehicleProfile::load(&car_model)?;
    info!("profile: {}", profile.name);

    let adapter = BluetoothElm327Adapter::new(&mac, profile)?;
    let publisher = Publisher::new(cfg.daemon.api_base_url.clone(), cfg.vehicle.battery_capacity_wh);

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("Ctrl-C handler");
    }

    scheduler::run(adapter, cfg.daemon, publisher, running).await
}
