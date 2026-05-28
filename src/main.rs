extern crate ctrlc;
use clap::Parser;
use log::info;
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
use crate::adapter::Adapter;
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

    let profile = match cfg.device.kind {
        DeviceType::Bluetooth | DeviceType::Usb => {
            let name = cfg.vehicle.profile.clone()
                .ok_or_else(|| anyhow::anyhow!("vehicle.profile required for ELM327 device types"))?;
            let p = VehicleProfile::load(&name)?;
            info!("profile: {}", p.name);
            Some(p)
        }
        DeviceType::Wican => None,
    };

    let adapter: Box<dyn Adapter> = match cfg.device.kind {
        DeviceType::Bluetooth => {
            let mac = cfg.device.bt_mac.clone()
                .ok_or_else(|| anyhow::anyhow!("device.bt_mac required for type='bluetooth'"))?;
            let prof = profile.expect("profile present for ELM327 types");
            Box::new(BluetoothElm327Adapter::new(&mac, prof, cfg.device.bt_passkey)?)
        }
        DeviceType::Usb => {
            let port = cfg.device.usb_port.clone()
                .ok_or_else(|| anyhow::anyhow!("device.usb_port required for type='usb'"))?;
            let baud = cfg.device.usb_baud.unwrap_or(115200);
            let prof = profile.expect("profile present for ELM327 types");
            Box::new(crate::adapter::usb::UsbElm327Adapter::new(&port, baud, prof)?)
        }
        DeviceType::Wican => {
            let mac = cfg.device.wican_mac.clone()
                .ok_or_else(|| anyhow::anyhow!("device.wican_mac required for type='wican'"))?;
            let max_retries  = cfg.device.wican_max_connect_retries.unwrap_or(5);
            let timeout_secs = cfg.device.wican_timeout_secs.unwrap_or(10);
            Box::new(crate::adapter::wican::WicanAdapter::new(
                &mac, cfg.device.wican_passkey, max_retries, timeout_secs)?)
        }
    };
    let publisher = Publisher::new(
        cfg.daemon.api_base_url.clone(),
        cfg.vehicle.battery_capacity_wh,
        cfg.daemon.publish_failure_threshold,
        cfg.daemon.publish_breaker_secs,
    );

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("Ctrl-C handler");
    }

    scheduler::run(adapter, cfg.daemon, publisher, running).await
}
