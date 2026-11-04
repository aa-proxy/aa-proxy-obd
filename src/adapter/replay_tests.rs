// Offline replay tests: shipped profiles driven through Elm327Session against
// the ELM327 emulator, fed with real recorded ECU responses.
//
// Data sources:
//   testdata/obdb_vectors.json - OBDb test-case recordings from the OBDb
//     repositories (https://github.com/OBDb/<name>) Hyundai-IONIQ-5 2477928,
//     Hyundai-Kona-Electric f898afb, Kia-EV6 b6fb1f7, Kia-Niro-EV 8cd9845,
//     MG-MG4 a1bb5c0, Renault-ZOE 5c01658, with OBDb's expected values
//     converted to profile units.
//   LIVE_EV6 below - a 2023 EV6 adapter session log (2026-05-14).
//
// The emulator runs on tokio's paused clock, so timeouts and delays elapse
// without real waiting.

use super::elm327::{extract_value, Elm327Session, DEFAULT_INIT};
use super::elm327_emu::{self as emu, EmuConfig, Ecu};
use super::Metrics;
use crate::profile::{Source, UdsPidSource, VehicleProfile};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Deserialize)]
struct VectorFile { vectors: Vec<Vector> }

#[derive(Deserialize, Clone)]
struct Vector {
    id: String,
    profile: String,
    profile_file: String,
    pid: String,
    request_tx: String,
    response_rx: String,
    frames: Vec<String>,
    expected: HashMap<String, f32>,
}

fn vectors() -> Vec<Vector> {
    let raw = include_str!("../../testdata/obdb_vectors.json");
    serde_json::from_str::<VectorFile>(raw).unwrap().vectors
}

fn profile(name: &str) -> VehicleProfile {
    let path = format!("{}/profiles/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// The init list a shipped profile declares.
fn profile_init(p: &VehicleProfile) -> Vec<&str> {
    p.elm327.as_ref().unwrap().init.as_ref().unwrap().iter().map(String::as_str).collect()
}

fn uds_source<'a>(p: &'a VehicleProfile, pid: &str) -> &'a UdsPidSource {
    p.sources.iter().find_map(|s| match s {
        Source::UdsPid(u) if u.pid.eq_ignore_ascii_case(pid) => Some(u),
        _ => None,
    }).unwrap_or_else(|| panic!("profile {} has no source for {pid}", p.name))
}

fn hexid(s: &str) -> u32 { u32::from_str_radix(s, 16).unwrap() }

fn ecu_for(v: &Vector) -> Ecu {
    let frames: Vec<&str> = v.frames.iter().map(String::as_str).collect();
    // Answer functional (7DF) requests too: the most permissive assumption
    // about how the real ECU behaves.
    Ecu::new(hexid(&v.request_tx), hexid(&v.response_rx)).functional().respond(&v.pid, &frames)
}

/// Per-source step of `poll_sources` (without error classification), for one
/// uds_pid source. Returns the extracted metrics or the error text.
async fn poll_source<T>(s: &mut Elm327Session<T>, uds: &UdsPidSource) -> Result<Metrics, String>
where T: AsyncRead + AsyncWrite + Unpin,
{
    let payload = if uds.multiframe {
        s.poll_uds_pid_multiframe(uds).await
    } else {
        s.poll_uds_pid(uds).await
    }.map_err(|e| format!("{e:#}"))?;
    let mut m = Metrics::new();
    for f in &uds.fields {
        if let Some(v) = extract_value(&payload, f) { m.insert(f.name.clone(), v); }
    }
    Ok(m)
}

fn close(a: f32, b: f32) -> bool { (a - b).abs() <= 0.02_f32.max(b.abs() * 1e-4) }

fn compare(id: &str, got: &Result<Metrics, String>, want: &HashMap<String, f32>) -> Vec<String> {
    let mut bad = Vec::new();
    match got {
        Err(e) => bad.push(format!("{id}: error {e}")),
        Ok(m) => for (k, w) in want {
            match m.get(k) {
                Some(g) if close(*g, *w) => {}
                g => bad.push(format!("{id}: {k} got {g:?} want {w}")),
            }
        },
    }
    bad
}

async fn replay(v: &Vector, uds: &UdsPidSource) -> Result<Metrics, String> {
    replay_with(ecu_for(v), uds).await
}

async fn replay_with(ecu: Ecu, uds: &UdsPidSource) -> Result<Metrics, String> {
    let (host, _log) = emu::spawn(EmuConfig::default(), vec![ecu]);
    let mut s = Elm327Session::new(host);
    s.run_init(DEFAULT_INIT).await.map_err(|e| format!("{e:#}"))?;
    poll_source(&mut s, uds).await
}

// ---- Real-data decode through the shipped profiles ------------------------

#[tokio::test(start_paused = true)]
async fn obdb_recordings_decode_through_shipped_profiles() {
    let mut bad = Vec::new();
    let mut checked = 0;
    let mut profiles: HashMap<String, VehicleProfile> = HashMap::new();
    for v in vectors().iter().filter(|v| v.profile != "niro" && v.profile != "zoe") {
        let p = profiles.entry(v.profile_file.clone()).or_insert_with(|| profile(&v.profile_file));
        let got = replay(v, uds_source(p, &v.pid)).await;
        bad.extend(compare(&v.id, &got, &v.expected));
        checked += 1;
    }
    assert!(checked >= 100, "only {checked} vectors");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

#[tokio::test(start_paused = true)]
async fn zoe_decode_matches_ze50_recordings() {
    // OBDb records the ZE50 (2021) on 29-bit DADA/DA; the shipped profile
    // addresses the Phase 1 EVC on 7E4/7EC. Only the decode is comparable, so
    // the recorded frames are served on the profile's addresses.
    let p = profile("zoe");
    let mut bad = Vec::new();
    let mut checked = 0;
    for v in vectors().iter().filter(|v| v.profile == "zoe") {
        let uds = uds_source(&p, &v.pid);
        let frames: Vec<&str> = v.frames.iter().map(String::as_str).collect();
        let ecu = Ecu::new(hexid(&uds.ecu_tx), hexid(&uds.ecu_rx)).respond(&v.pid, &frames);
        bad.extend(compare(&v.id, &replay_with(ecu, uds).await, &v.expected));
        checked += 1;
    }
    assert!(checked > 0, "no zoe vectors");
    assert!(bad.is_empty(), "{}", bad.join("
"));
}

#[tokio::test(start_paused = true)]
async fn niro_shipped_profile_receives_bms_reply() {
    let p = profile("niro");
    let mut bad = Vec::new();
    let mut total = 0;
    for v in vectors().iter().filter(|v| v.profile == "niro") {
        bad.extend(compare(&v.id, &replay(v, uds_source(&p, &v.pid)).await, &v.expected));
        total += 1;
    }
    assert!(total > 0, "no niro recordings were replayed");
    assert!(bad.is_empty(), "{} of {total} recordings failed, first: {:?}", bad.len(), bad.first());
}

// Live replies captured from a real adapter (headers off, spaces off) on a
// 2023 EV6; expected values are the probe's own decode. The log stored lines
// LF-joined with the prompt stripped, so CR line ends and '>' are restored.
type LiveReply = (&'static str, &'static str, &'static [(&'static str, f32)]);

const LIVE_EV6: &[LiveReply] = &[
    ("220105",
     "02E\r0:620105FFFB74\r1:0F012C01012C12\r2:1011101110125D\r3:E99FB500005011\r4:0003E8B3691400\r5:9A000000000000\r6:0010120F11AAAA\r\r>",
     &[("battery_level_percentage", 77.0), ("battery_level_wh", 53800.0), ("battery_state_of_health", 100.0)]),
    ("22B002",
     "00F\r0:62B002E00000\r1:00FFAF00517B00\r2:0000AAAAAAAAAA\r\r>",
     &[("odometer_km", 20859.0)]),
    ("22C00B",
     "024\r0:62C00BFFFFFF\r1:80B042000400AE\r2:43000500B34200\r3:0400AE43000400\r4:FF85A685A698BF\r5:98BFAAAAAAAAAA\r\r>",
     &[("tire_fl_kpa", 35.2 * 6.89476), ("tire_fr_kpa", 34.8 * 6.89476),
       ("tire_rl_kpa", 35.8 * 6.89476), ("tire_rr_kpa", 34.8 * 6.89476)]),
];

#[test]
fn live_ev6_adapter_text_decodes_through_profile() {
    let p = profile("ev6");
    let mut bad = Vec::new();
    for (pid, raw, want) in LIVE_EV6 {
        let payload = super::elm327::get_payload(raw);
        let uds = uds_source(&p, pid);
        let mut m = Metrics::new();
        for f in &uds.fields {
            if let Some(v) = extract_value(&payload, f) { m.insert(f.name.clone(), v); }
        }
        for (k, w) in *want {
            match m.get(*k) {
                // probe values are rounded to 0.1 psi / 0.1 kWh
                Some(g) if (g - w).abs() <= w.abs() * 0.005 + 0.05 => {}
                g => bad.push(format!("{pid}: {k} got {g:?} want {w}")),
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---- Bluetooth connect: probe CR, then init -------------------------------

/// A recorded Kona Electric 220105 reply, decoded with kona.json's 220105
/// source.
fn kona_220105() -> Vector {
    vectors().into_iter()
        .find(|v| v.profile == "kona" && v.pid == "220105" && v.expected.len() >= 2)
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn atz_with_non_elm_banner_adds_no_wait() {
    let v = kona_220105();
    let cfg = EmuConfig { banner: "OBDII v1.5".into(), reset_ms: 1000, ..Default::default() };
    let (host, _log) = emu::spawn(cfg, vec![ecu_for(&v)]);
    let mut s = Elm327Session::new(host);
    let start = tokio::time::Instant::now();
    s.run_init(DEFAULT_INIT).await.unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(2), "init took {elapsed:?}");
    let p = profile(&v.profile_file);
    let res = poll_source(&mut s, uds_source(&p, &v.pid)).await;
    let bad = compare(&v.id, &res, &v.expected);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---- Multi-frame path (zoe 2174) -----------------------------------------

/// Zoe 2174 reply. No raw recording exists; the tire bytes default to the raw
/// values behind the pressures in a Renault Zoe capture
/// (fl 278.6175, fr 267.63748, rl 269.00998, rr 273.1275 kPa = 203/195/196/199
/// x 1.3725). Tire bytes sit at header-relative indices 11..=14 per
/// profiles/zoe.json, in rr, rl, fr, fl order; the rest of the payload is filler.
const ZOE_CAPTURE_TIRES: [u8; 4] = [199, 196, 195, 203];

fn zoe_tpms_payload(tires: [u8; 4]) -> String {
    let mut b = vec![0x61, 0x74];
    b.extend_from_slice(&[0x01; 12]);
    b.extend_from_slice(&tires);
    b.extend_from_slice(&[0x00; 2]);
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Run two full Zoe cycles over the uds sources. Replies are rebuilt from the
/// values a 2026-05-29 Renault Zoe log shows (SOC 51.96 %, 10705 Wh,
/// 136452 km); the raw bytes are derived, not recorded.
/// Results are returned in poll order, each tagged with its source's PID.
async fn zoe_cycle(cfg: EmuConfig, tires: [u8; 4]) -> (Vec<(String, Result<Metrics, String>)>, Vec<(String, bool)>) {
    let ecus = vec![
        Ecu::new(0x745, 0x765).respond_payload("2174", &zoe_tpms_payload(tires)),
        Ecu::new(0x7E4, 0x7EC)
            .respond_payload("10C0", "50 C0")
            .respond_payload("222002", "62 20 02 0A 26")
            .respond_payload("222006", "62 20 06 02 15 04")
            .respond_payload("22320C", "62 32 0C 08 5D"),
    ];
    let p = profile("zoe");
    let init = profile_init(&p);
    let (host, log) = emu::spawn(cfg, ecus);
    let mut s = Elm327Session::new(host);
    s.run_init(&init).await.unwrap();
    let mut results = Vec::new();
    for _ in 0..2 {
        for src in &p.sources {
            if let Source::UdsPid(u) = src { results.push((u.pid.clone(), poll_source(&mut s, u).await)); }
        }
    }
    let log = log.lock().unwrap().clone();
    (results, log)
}

/// Expected metrics per PID for one cycle, repeated for the two cycles run.
fn zoe_expected(tires: [u8; 4]) -> Vec<(String, HashMap<String, f32>)> {
    let t = |r: u8| r as f32 * 1.3725;
    let one: Vec<(String, HashMap<String, f32>)> = vec![
        ("222002".to_string(), [("battery_level_percentage".to_string(), 51.96)].into()),
        ("222006".to_string(), [("odometer_km".to_string(), 136452.0)].into()),
        ("22320C".to_string(), [("battery_level_wh".to_string(), 10705.0)].into()),
        ("2174".to_string(), [("tire_rr_kpa".to_string(), t(tires[0])), ("tire_rl_kpa".to_string(), t(tires[1])),
         ("tire_fr_kpa".to_string(), t(tires[2])), ("tire_fl_kpa".to_string(), t(tires[3]))].into()),
    ];
    [one.clone(), one].concat()
}

fn check_zoe(results: &[(String, Result<Metrics, String>)], log: &[(String, bool)], tires: [u8; 4]) {
    let expected = zoe_expected(tires);
    assert_eq!(results.len(), expected.len(), "result count; log: {log:?}");
    let mut bad = Vec::new();
    for (i, ((pid, r), (want_pid, w))) in results.iter().zip(&expected).enumerate() {
        if pid != want_pid {
            bad.push(format!("poll#{i}: got pid {pid}, want {want_pid}"));
            continue;
        }
        bad.extend(compare(&format!("poll#{i} {pid}"), r, w));
    }
    assert!(bad.is_empty(), "{}\nlog: {log:?}", bad.join("\n"));
}

#[tokio::test(start_paused = true)]
async fn zoe_cycles_with_post_reply_wait() {
    // The adapter keeps waiting for further replies for 150 ms after the
    // last frame, so the prompt is delayed. The 2174 read ends about 500 ms
    // after the data (the final prompt-only read timing out) and the
    // success-path restore (ATH0, ATS1, ATCRA) follows.
    let cfg = EmuConfig { interrupt_aborts: true, post_reply_wait_ms: 150, ..Default::default() };
    let (results, log) = zoe_cycle(cfg, ZOE_CAPTURE_TIRES).await;
    check_zoe(&results, &log, ZOE_CAPTURE_TIRES);
}

#[tokio::test(start_paused = true)]
async fn multiframe_negative_response_is_an_error() {
    let ecus = vec![
        Ecu::new(0x745, 0x765).respond_payload("2174", "7F 21 12"),
        Ecu::new(0x7E4, 0x7EC).respond_payload("222002", "62 20 02 0A 26"),
    ];
    let p = profile("zoe");
    let init = profile_init(&p);
    let (host, log) = emu::spawn(EmuConfig::default(), ecus);
    let mut s = Elm327Session::new(host);
    s.run_init(&init).await.unwrap();
    let r = poll_source(&mut s, uds_source(&p, "2174")).await;
    // The session stays in step: the next single-frame poll decodes normally.
    let soc = poll_source(&mut s, uds_source(&p, "222002")).await;
    let log = log.lock().unwrap().clone();
    assert!(r.as_ref().is_err_and(|e| e.contains("UDS error response")), "{r:?}\nlog: {log:?}");
    let bad = compare("222002", &soc, &[("battery_level_percentage".to_string(), 51.96)].into());
    assert!(bad.is_empty(), "{}\nlog: {log:?}", bad.join("\n"));
    let req = log.iter().rposition(|(c, _)| c == "2174").expect("2174 was sent");
    assert!(log[req..].iter().any(|(c, acted)| c == "ATH0" && *acted), "headers not restored\nlog: {log:?}");
}

/// A frame gap longer than the 500 ms per-line read ends the multiframe read
/// before the reply's prompt arrives. The rest of the reply and its prompt are
/// consumed, so the restore commands and the next poll read their own replies.
#[tokio::test(start_paused = true)]
async fn multiframe_read_timeout_keeps_session_in_step() {
    let ecus = vec![
        Ecu::new(0x745, 0x765).respond_payload("2174", &zoe_tpms_payload(ZOE_CAPTURE_TIRES)),
        Ecu::new(0x7E4, 0x7EC)
            .respond_payload("10C0", "50 C0")
            .respond_payload("222002", "62 20 02 0A 26"),
    ];
    let cfg = EmuConfig { line_delay_ms: 600, interrupt_aborts: false, ..Default::default() };
    let p = profile("zoe");
    let init = profile_init(&p);
    let (host, log) = emu::spawn(cfg, ecus);
    let mut s = Elm327Session::new(host);
    s.run_init(&init).await.unwrap();
    let _ = poll_source(&mut s, uds_source(&p, "2174")).await;
    let soc = poll_source(&mut s, uds_source(&p, "222002")).await;
    let log = log.lock().unwrap().clone();
    let bad = compare("222002", &soc, &[("battery_level_percentage".to_string(), 51.96)].into());
    assert!(bad.is_empty(), "{}\nlog: {log:?}", bad.join("\n"));
    let req = log.iter().rposition(|(c, _)| c == "2174").expect("2174 was sent");
    for cmd in ["ATH0", "ATS1", "ATCRA"] {
        assert!(
            log[req..].iter().any(|(c, acted)| c.starts_with(cmd) && *acted),
            "{cmd} not acted on after 2174\nlog: {log:?}"
        );
    }
}

// ---- Broadcast scan followed by polling ---------------------------------------

/// The ELM answers the character that stops ATMA after a delay (it completes
/// the line it is printing first) while each command takes about 40 ms. The
/// prompt for that stop must be consumed by monitor_broadcast, so the polls
/// that follow read their own replies. The session keeps one reader, so a reply
/// that arrives in the same read as a stray prompt is not lost either way.
#[tokio::test(start_paused = true)]
async fn broadcast_then_poll_stays_in_sync() {
    let ecus = vec![
        Ecu::new(0x7E4, 0x7EC)
            .respond_payload("10C0", "50 C0")
            .respond_payload("222002", "62 20 02 0A 26")
            .respond_payload("222006", "62 20 06 02 15 04")
            .respond_payload("22320C", "62 32 0C 08 5D"),
    ];
    let cfg = EmuConfig {
        broadcast: vec![(0x1F6, vec![0x01; 8]), (0x656, vec![0, 0, 0, 0, 0, 0, 0x40, 0])],
        monitor_stop_delay_ms: 120,
        ..Default::default()
    };
    let p = profile("zoe");
    let init = profile_init(&p);
    let (host, log) = emu::spawn(cfg, ecus);
    let mut s = Elm327Session::new(host);
    s.run_init(&init).await.unwrap();
    let spec = p.sources.iter().find_map(|src| match src {
        Source::Broadcast(b) => Some(b),
        _ => None,
    }).unwrap();
    let scan = s.monitor_broadcast(spec).await.unwrap();
    let soc = poll_source(&mut s, uds_source(&p, "222002")).await;
    let odo = poll_source(&mut s, uds_source(&p, "222006")).await;
    let mut bad = compare("scan", &Ok(scan), &[("external_temp_celsius".to_string(), 24.0)].into());
    bad.extend(compare("222002", &soc, &[("battery_level_percentage".to_string(), 51.96)].into()));
    bad.extend(compare("222006", &odo, &[("odometer_km".to_string(), 136452.0)].into()));
    let log = log.lock().unwrap().clone();
    assert!(bad.is_empty(), "{}\nlog: {log:?}", bad.join("\n"));
}

// ---- Zoe SOC (shipped 3-byte form vs plain 2-byte form) ---

/// A peer that answers every AT command with OK and, once it gets the request,
/// prints a non-empty line every 400 ms and never a prompt. The poll gives up at
/// its overall deadline instead of reading until the line cap.
#[tokio::test(start_paused = true)]
async fn multiframe_reply_without_prompt_ends_at_the_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let p = profile("zoe");
    let uds = uds_source(&p, "2174").clone();
    let (host, mut peer) = tokio::io::duplex(1024);
    tokio::spawn(async move {
        let mut pending = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if peer.read(&mut byte).await.unwrap_or(0) == 0 { return; }
            if byte[0] != b'\r' { pending.push(byte[0]); continue; }
            let cmd = String::from_utf8_lossy(&pending).into_owned();
            pending.clear();
            if cmd.starts_with("AT") {
                if peer.write_all(b"OK\r\r>").await.is_err() { return; }
            } else {
                loop {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    if peer.write_all(b"61 74 00 01 02 03\r").await.is_err() { return; }
                }
            }
        }
    });
    let mut s = Elm327Session::new(host);
    let start = tokio::time::Instant::now();
    let _ = s.poll_uds_pid_multiframe(&uds).await;
    let took = start.elapsed();
    assert!(took < Duration::from_secs(30), "poll took {took:?}");
}
