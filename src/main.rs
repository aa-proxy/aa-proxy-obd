extern crate ctrlc;
use clap::Parser;
use log::{error, info};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod adapter;
mod cli {
    pub mod pair;
}
mod config;
mod logging;
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
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long)] debug: bool,
    #[arg(short, long, default_value = "/etc/aa-proxy-obd.toml")] config: PathBuf,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Pair the configured Bluetooth device and optionally save the passkey.
    Pair {
        /// Passkey to provide during pairing. If omitted, prompts on stdin.
        #[arg(long)]
        passkey: Option<u32>,
    },
}


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(Commands::Pair { passkey }) = &args.command {
        logging::init("info", "/dev/null", args.debug);
        return cli::pair::run(&args.config, *passkey).await;
    }

    let cfg = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => { eprintln!("aa-proxy-obd: config load failed: {e}"); std::process::exit(1); }
    };
    logging::init(&cfg.daemon.log_level, &cfg.daemon.log_file, args.debug);
    info!("aa-proxy-obd started");

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

    let adapter = BluetoothElm327Adapter::new(&mac, profile, cfg.device.bt_passkey)?;
    let publisher = Publisher::new(cfg.daemon.api_base_url.clone(), cfg.vehicle.battery_capacity_wh);

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("Ctrl-C handler");
    }

    scheduler::run(adapter, cfg.daemon, publisher, running).await
}
