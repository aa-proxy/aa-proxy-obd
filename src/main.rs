extern crate ctrlc;
use bluer::{
    rfcomm::{SocketAddr, Stream},
    Address,
};
use clap::Parser;
use ini::Ini;
use simplelog::*;
use std::io::{self, Error, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use std::collections::HashMap;

mod config;
use config::{VehicleConfig, PidConfig, MetricConfig};

// Secs between polling
pub const POLL_INTERVAL_SECS: f32 = 10.0;
// Secs between polling when car is in sleep mode or is not in range
pub const CAR_SLEEP_INTERVAL_SECS: f32 = 100.0;

// Universal ELM327 initialization commands
const INIT: &[&str] = &["ATZ", "ATE0", "ATAL", "ATST96", "ATCP18", "ATFCSD300000", "ATSP6"];
const _EOM1: u8 = b'\r';
const EOM2: u8 = b'>';
const _EOM3: u8 = b'?';

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

use reqwest::Client;
use serde::Serialize;

#[derive(Debug, Serialize, Default, Clone)]
struct BatteryData {
    #[serde(skip_serializing_if = "Option::is_none")]
    battery_level_percentage: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_temp_celsius: Option<f32>,
}

#[derive(Debug, Serialize, Default, Clone)]
struct OdometerData {
    odometer_km: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    trip_km: Option<f32>,
}

#[derive(Debug, Serialize, Default, Clone)]
struct TirePressureData {
    pressures_kpa: Vec<f32>,
}

/// Simple daemon to read Renault Zoe basic parameters using
/// bluetooth dongle and publish to REST
#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
struct Args {
    /// Enable debug info
    #[clap(short, long)]
    debug: bool,

    /// Config file path
    #[clap(short, long, parse(from_os_str), default_value = "/etc/canze-rs.conf")]
    config: std::path::PathBuf,
}

fn logging_init(debug: bool) {
    let conf = ConfigBuilder::new()
        .set_time_format("%F, %H:%M:%S%.3f".to_string())
        .set_write_log_enable_colors(true)
        .build();

    let mut loggers = vec![];

    let console_logger: Box<dyn SharedLogger> = TermLogger::new(
        if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        },
        conf.clone(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );
    loggers.push(console_logger);

    CombinedLogger::init(loggers).expect("Cannot initialize logging subsystem");
}

fn get_config_string(conf: &Ini, option_name: &str, section: Option<&str>) -> io::Result<String> {
    conf.section(Some(section.unwrap_or("general").to_owned()))
        .and_then(|x| x.get(option_name).cloned())
        .ok_or(Error::new(
            ErrorKind::NotFound,
            format!("No config entry for: `{}` in section `[{}]`", option_name, section.unwrap_or("general")),
        ))
}

pub async fn send_cmd(stream: &mut Stream, cmd: String) -> io::Result<Option<Vec<u8>>> {

    let mut buffer = Vec::new(); // Must be empty - read_until APPENDS



    let mut output_cmd: Vec<u8> = vec![];
    let out: Option<Vec<u8>>;

    output_cmd.extend(cmd.as_bytes());
    output_cmd.push(b'\r');
    debug!("write: {}", String::from_utf8_lossy(&output_cmd));
    if let Err(e) = stream.write_all(&output_cmd).await {
        error!("write error: {:?}", e);
        return Err(e.into());
    }

    let mut packet = BufReader::new(stream);
    let retval = packet.read_until(EOM2, &mut buffer);
    match timeout(Duration::from_secs_f32(5.0), retval).await {
        Ok(res) => match res {
            Ok(len) => {
                if len == 0 {
                    error!("file read error: 0 bytes");
                    return Err(Error::new(ErrorKind::Other, "0 bytes read"));
                }
                out = Some(buffer.clone());
                trace!("Response: {:?}", buffer);
                let ascii = String::from_utf8_lossy(&buffer);
                debug!("Response ASCII (len={}): {}", len, ascii);
                if ascii.contains("NO DATA") {
                    return Err(Error::new(ErrorKind::Other, "no data"));
                }
                if ascii.contains("7F 22 12") {
                    return Err(Error::new(ErrorKind::Other, "Service Not Supported"));
                }
            }
            Err(e) => {
                error!("file read error: {}", e);
                return Err(e.into());
            }
        },
        Err(e) => {
            error!("response timeout: {}", e);
            return Err(e.into());
        }
    }

    Ok(out)
}

pub fn get_payload(response: &str) -> Vec<u8> {
    let frames: Vec<&str> = response.split(|c| c == '\r' || c == '\n')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| !s.contains("SEARCHING"))
        .collect();

    let mut payload = Vec::new();
    let mut is_first = true;

    for frame in frames {
        let cleaned = frame.replace(" ", "");
        
        // Skip purely length headers from ELM327 (e.g. "014")
        if is_first && cleaned.len() <= 3 {
            is_first = false;
            continue;
        }
        is_first = false;

        let mut data_str = if cleaned.contains(':') {
            cleaned.split(':').nth(1).unwrap().to_string()
        } else {
            cleaned.to_string()
        };
        
        let mut bytes = Vec::new();
        let mut chars = data_str.chars();
        while let (Some(c1), Some(c2)) = (chars.next(), chars.next()) {
            let hex_str = format!("{}{}", c1, c2);
            if let Ok(b) = u8::from_str_radix(&hex_str, 16) {
                bytes.push(b);
            }
        }

        if bytes.is_empty() {
            continue;
        }

        payload.extend(bytes);
    }
    payload
}

pub fn extract_value(payload: &[u8], metric: &MetricConfig) -> Option<f32> {
    let idx = if metric.byte_index < 0 {
        let positive_idx = payload.len() as i32 + metric.byte_index;
        if positive_idx < 0 { return None; }
        positive_idx as usize
    } else {
        (metric.byte_index + 3) as usize
    };

    if idx + metric.length > payload.len() {
        return None;
    }

    let raw_val = if metric.signed.unwrap_or(false) {
        match metric.length {
            1 => payload[idx] as i8 as f32,
            2 => i16::from_be_bytes([payload[idx], payload[idx+1]]) as f32,
            3 => {
                // Sign extend 24-bit to 32-bit
                let mut val = ((payload[idx] as u32) << 16) | ((payload[idx+1] as u32) << 8) | (payload[idx+2] as u32);
                if val & 0x800000 != 0 {
                    val |= 0xFF000000;
                }
                val as i32 as f32
            },
            _ => return None,
        }
    } else {
        match metric.length {
            1 => payload[idx] as f32,
            2 => u16::from_be_bytes([payload[idx], payload[idx+1]]) as f32,
            3 => {
                let val = ((payload[idx] as u32) << 16) | ((payload[idx+1] as u32) << 8) | (payload[idx+2] as u32);
                val as f32
            },
            _ => return None,
        }
    };

    Some((raw_val * metric.multiplier) + metric.offset)
}

pub async fn get_raw_pid(
    stream: &mut Stream,
    p: &PidConfig,
) -> io::Result<Vec<u8>> {
    let cmd = format!("ATSH{}\r", p.ecu_tx);
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATCRA{}\r", p.ecu_rx);
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATFCSH{}\r", p.ecu_tx);
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATFCSD300000\r");
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATFCSM1\r");
    send_cmd(stream, cmd).await?;
    // Send optional pre-request (e.g. UDS extended session "10C0" for Renault Zoe).
    // Errors are intentionally ignored — some adapters/ECUs may not ACK this.
    if let Some(pre) = &p.pre_request {
        let _ = send_cmd(stream, format!("{}\r", pre)).await;
    }
    let cmd = format!("{}\r", p.pid);
    let out = send_cmd(stream, cmd).await?.unwrap();
    let raw_string = String::from_utf8_lossy(&out);

    Ok(get_payload(&raw_string))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging_init(args.debug);
    info!("<b><blue>canze-rs</> started");
    info!("Using config file: <b><blue>{:?}</>", args.config);
    let conf = match Ini::load_from_file(args.config) {
        Ok(c) => c,
        Err(e) => {
            error!("Cannot open config file: {}", e);
            return Ok(());
        }
    };
    let mac = get_config_string(&conf, "mac", Some("general"))?;
    let car_model = get_config_string(&conf, "car", Some("general"))?;

    info!("Configured for car model: <b><green>{}</>", &car_model);
    
    // Load vehicle JSON profile
    let vehicle_cfg = match VehicleConfig::load(&car_model) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load vehicle JSON profile: {}", e);
            return Err(e.into());
        }
    };
    info!("Successfully loaded profile for: {}", vehicle_cfg.name);

    // Parse target mac address for bluetooth
    let target_addr: Address = mac.parse().expect("invalid address");
    let target_sa = SocketAddr::new(target_addr, 1u8);

    // Ctrl-C / SIGTERM support
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let mut poll_interval = Instant::now();
    let client = Client::new();

    'connect: loop {
        if !running.load(Ordering::SeqCst) {
            info!("🛑 Ctrl-C or SIGTERM signal detected, exiting...");
            break;
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
        info!("Connecting to: {:?}", &target_sa);
        let res = Stream::connect(target_sa).await;
        let mut stream = if let Ok(s) = res {
            s
        } else {
            info!("Cannot connect (BT dongle not in range?)");
            continue;
        };

        // The following code is a workaround for a problem described here:
        // https://github.com/bluez/bluer/discussions/130#discussioncomment-8845113
        debug!("Local address before: {:?}", stream.as_ref().local_addr()?);
        let mut i = 0;
        while stream.as_ref().local_addr()?.addr == bluer::Address::any() {
            debug!("Waiting for local address...");
            tokio::time::sleep(Duration::from_secs(1)).await;
            i += 1;
            if i > 5 {
                break;
            }
        }

        info!("connected, poll interval: {}s", POLL_INTERVAL_SECS);

        for s in INIT {
            if let Err(_) = send_cmd(&mut stream, s.to_string()).await {
                info!("INIT error, reconnecting");
                continue 'connect;
            }
        }

        'inner: loop {
            if !running.load(Ordering::SeqCst) {
                continue 'connect;
            }

            if poll_interval.elapsed() > Duration::from_secs(0) {
                poll_interval = Instant::now() + Duration::from_secs_f32(POLL_INTERVAL_SECS);

                let mut metrics_map: HashMap<String, f32> = HashMap::new();

                for pid_cfg in &vehicle_cfg.pids {
                    debug!("Trying to obtain PID: {}", pid_cfg.pid);
                    match get_raw_pid(&mut stream, pid_cfg).await {
                        Ok(payload) => {
                            if payload.is_empty() { continue; }
                            
                            for metric in &pid_cfg.fields {
                                if let Some(val) = extract_value(&payload, metric) {
                                    info!("Extracted {}: {}", metric.name, val);
                                    metrics_map.insert(metric.name.clone(), val);
                                } else {
                                    warn!("Failed to extract {} (bounds check failed)", metric.name);
                                }
                            }
                        }
                        Err(e) => {
                            info!("GET PARAM error for {}: {:?}", pid_cfg.pid, e);
                            if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                                info!("CAN network down / car is sleeping... waiting 100s");
                                poll_interval = Instant::now() + Duration::from_secs_f32(CAR_SLEEP_INTERVAL_SECS);
                                continue 'inner;
                            }
                            if e.kind() == std::io::ErrorKind::BrokenPipe
                                || e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::NotConnected
                            {
                                info!("Broken pipe/TimedOut/NotConnected detected... reconnecting");
                                continue 'connect;
                            }
                        }
                    }
                }

                // POST Battery
                if metrics_map.contains_key("battery_level_percentage") || metrics_map.contains_key("external_temp_celsius") {
                    let data = BatteryData {
                        battery_level_percentage: metrics_map.get("battery_level_percentage").copied(),
                        external_temp_celsius: metrics_map.get("external_temp_celsius").copied(),
                    };
                    if let Err(e) = client.post("http://localhost/battery").json(&data).send().await {
                        warn!("Failed to POST /battery: {}", e);
                    }
                }

                // POST Odometer
                if let Some(&odo) = metrics_map.get("odometer_km") {
                    let data = OdometerData { odometer_km: odo, trip_km: None };
                    if let Err(e) = client.post("http://localhost/odometer").json(&data).send().await {
                        warn!("Failed to POST /odometer: {}", e);
                    }
                }

                // POST TPMS
                if metrics_map.contains_key("tire_fl_kpa") {
                    let data = TirePressureData {
                        pressures_kpa: vec![
                            metrics_map.get("tire_fl_kpa").copied().unwrap_or(0.0),
                            metrics_map.get("tire_fr_kpa").copied().unwrap_or(0.0),
                            metrics_map.get("tire_rl_kpa").copied().unwrap_or(0.0),
                            metrics_map.get("tire_rr_kpa").copied().unwrap_or(0.0),
                        ]
                    };
                    if let Err(e) = client.post("http://localhost/tire-pressure").json(&data).send().await {
                        warn!("Failed to POST /tire-pressure: {}", e);
                    }
                }
                
                debug!("Got all params, sleeping 10 secs for next cycle");
            }

            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    Ok(())
}
