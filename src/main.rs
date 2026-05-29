extern crate ctrlc;
use clap::builder::styling::{AnsiColor, Styles};
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

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default().bold())
    .usage(AnsiColor::Yellow.on_default().bold())
    .literal(AnsiColor::Green.on_default().bold())
    .placeholder(AnsiColor::Cyan.on_default());

#[derive(Parser, Debug)]
#[command(version, about, long_about = None, styles = HELP_STYLES)]
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
    info!("<b><blue>aa-proxy-obd</> started");
    info!("Using config file: <b><blue>{:?}</>", args.config);

    let profile = match cfg.device.kind {
        DeviceType::Bluetooth | DeviceType::Usb => {
            let name = cfg.vehicle.profile.clone()
                .ok_or_else(|| anyhow::anyhow!("vehicle.profile required for ELM327 device types"))?;
            let p = VehicleProfile::load(&name)?;
            info!("Loaded profile: <b><green>{}</>", p.name);
            Some(p)
        }
        DeviceType::Wican => None,
    };

    // Shared Bluetooth connection options (apply to bluetooth + wican).
    let bt_retries = cfg.device.bt_max_connect_retries.unwrap_or(5);
    let bt_timeout = cfg.device.bt_timeout_secs.unwrap_or(10);

    let adapter: Box<dyn Adapter> = match cfg.device.kind {
        DeviceType::Bluetooth => {
            let mac = cfg.device.bt_mac.clone()
                .ok_or_else(|| anyhow::anyhow!("device.bt_mac required for type='bluetooth'"))?;
            let prof = profile.expect("profile present for ELM327 types");
            Box::new(BluetoothElm327Adapter::new(
                &mac, prof, cfg.device.bt_passkey, bt_retries, bt_timeout)?)
        }
        DeviceType::Usb => {
            let port = cfg.device.usb_port.clone()
                .ok_or_else(|| anyhow::anyhow!("device.usb_port required for type='usb'"))?;
            let baud = cfg.device.usb_baud.unwrap_or(115200);
            let prof = profile.expect("profile present for ELM327 types");
            Box::new(crate::adapter::usb::UsbElm327Adapter::new(&port, baud, prof)?)
        }
        DeviceType::Wican => {
            let mac = cfg.device.bt_mac.clone()
                .ok_or_else(|| anyhow::anyhow!("device.bt_mac required for type='wican'"))?;
            Box::new(crate::adapter::wican::WicanAdapter::new(
                &mac, cfg.device.bt_passkey, bt_retries, bt_timeout)?)
        }
    };
    let publisher = Publisher::new(
        cfg.daemon.api_base_url.clone(),
        cfg.vehicle.battery_capacity_wh,
        cfg.daemon.publish_failure_threshold,
        cfg.daemon.publish_breaker_secs,
        cfg.daemon.bridge_dropouts,
    );

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("Ctrl-C handler");
    }

    scheduler::run(adapter, cfg.daemon, publisher, running).await
}
