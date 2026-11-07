// Test-only ELM327 emulator. Runs on the far side of a tokio duplex pipe and
// answers like an ELM327 (v1.5/v2.x command set, CAN 11-bit) wired to a set of
// scripted ECUs. Behaviour follows the ELM327 datasheet:
//   - echo is ON after ATZ until ATE0
//   - a bare CR repeats the last command, or prints '?' when there is none
//   - input received while ATZ is resetting is lost
//   - `drop_first` loses the first received line equal to a given command
//     (models input lost while the chip is busy); off by default
//   - ATCRA filters replies by CAN-ID; ATSH selects the request header
//   - headers off + CAN auto-formatting prints a length line and "N:" lines
//     for ISO-TP multi-frame replies, and strips the PCI byte of single frames
//   - a character received while an OBD request is in progress (between
//     reply lines, or while waiting for further replies after the last
//     frame) interrupts it and is discarded (when `interrupt_aborts` is set);
//     the datasheet prints STOPPED only for an interrupted protocol search
//   - ATMA prints broadcast lines until a character arrives; that character
//     is discarded. `monitor_stop_delay_ms` is the time between it and the
//     prompt, during which one more monitored line is printed (the datasheet:
//     the IC completes the line it is printing, and the response time varies)
//   - the datasheet describes the 256-byte monitor buffer but does not state
//     that BUFFER FULL ends monitoring; `monitor_self_stop_after` models
//     adapters that do (BUFFER FULL, then the prompt, with no character sent)
//   - `monitor_close_after` closes the link after that many monitor lines
//     (peer gone mid-scan); off by default
//   - `slow_commands` adds latency to replies of the named commands only
//     (command uppercase, no spaces); empty by default
// Timing knobs (reply latency, reset time, per-line delay) let tests model
// slow clones and Bluetooth latency.

use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::{sleep, timeout, Instant};

#[derive(Clone, Debug)]
pub struct Ecu {
    /// Physical request header this ECU answers on (e.g. 0x7E4).
    pub tx: u32,
    /// CAN-ID the ECU replies from (e.g. 0x7EC).
    pub rx: u32,
    /// Also answer requests sent to the functional header 0x7DF.
    pub functional: bool,
    /// Request (uppercase hex, no spaces) -> raw CAN data frames, PCI included.
    pub responses: HashMap<String, Vec<Vec<u8>>>,
}

impl Ecu {
    pub fn new(tx: u32, rx: u32) -> Self {
        Self {
            tx,
            rx,
            functional: false,
            responses: HashMap::new(),
        }
    }
    pub fn functional(mut self) -> Self {
        self.functional = true;
        self
    }
    /// Add a response given as hex strings, one per CAN frame (PCI included).
    pub fn respond(mut self, request: &str, frames: &[&str]) -> Self {
        let frames: Vec<Vec<u8>> = frames.iter().map(|f| hex(f)).collect();
        for f in &frames {
            check_pci(request, f);
        }
        self.responses.insert(request.to_ascii_uppercase(), frames);
        self
    }
    /// Add a UDS/OBD response given as the plain payload ("62 01 05 ...");
    /// it is split into ISO-TP frames.
    pub fn respond_payload(mut self, request: &str, payload: &str) -> Self {
        self.responses
            .insert(request.to_ascii_uppercase(), isotp_frames(&hex(payload)));
        self
    }
}

#[derive(Clone, Debug)]
pub struct EmuConfig {
    pub banner: String,
    /// Time after ATZ during which input is ignored.
    pub reset_ms: u64,
    /// Delay before any reply starts (models clone/Bluetooth latency).
    pub reply_latency_ms: u64,
    /// Extra delay before the first reply only (slow first answer after the
    /// link comes up).
    pub first_reply_latency_ms: u64,
    /// Extra reply delay for individual commands: (command, ms). Commands
    /// compare uppercase with spaces removed.
    pub slow_commands: Vec<(String, u64)>,
    /// Delay between printed lines of a multi-line reply.
    pub line_delay_ms: u64,
    /// Input during an OBD request interrupts it.
    pub interrupt_aborts: bool,
    /// Fixed wait set by the test: models the ELM listening for further
    /// replies after the last frame before it prints the prompt.
    pub post_reply_wait_ms: u64,
    /// Bytes waiting in the stream before the host sends anything.
    pub stale_on_connect: Vec<u8>,
    /// Lose the first received line equal to this command: it is logged as
    /// not acted on and gets no reply.
    pub drop_first: Option<String>,
    /// CAN broadcast traffic printed by ATMA: (CAN-ID, data bytes).
    pub broadcast: Vec<(u32, Vec<u8>)>,
    /// Time between the character that stops ATMA and the prompt. With a
    /// non-zero value one more monitored line is printed first.
    pub monitor_stop_delay_ms: u64,
    /// End ATMA by itself after this many printed lines: "BUFFER FULL", then
    /// the prompt.
    pub monitor_self_stop_after: Option<usize>,
    /// Close the link after this many monitor lines, with no prompt: the
    /// host side then reads end-of-file.
    pub monitor_close_after: Option<usize>,
}

impl Default for EmuConfig {
    fn default() -> Self {
        Self {
            banner: "ELM327 v1.5".into(),
            reset_ms: 0,
            reply_latency_ms: 0,
            first_reply_latency_ms: 0,
            slow_commands: Vec::new(),
            line_delay_ms: 0,
            interrupt_aborts: false,
            post_reply_wait_ms: 0,
            stale_on_connect: Vec::new(),
            drop_first: None,
            broadcast: Vec::new(),
            monitor_stop_delay_ms: 0,
            monitor_self_stop_after: None,
            monitor_close_after: None,
        }
    }
}

/// Every command line the emulator received, in order, with whether it was
/// acted on (false = dropped during reset).
pub type CommandLog = std::sync::Arc<std::sync::Mutex<Vec<(String, bool)>>>;

struct State {
    echo: bool,
    headers: bool,
    spaces: bool,
    linefeeds: bool,
    sh: u32,
    cra: Option<u32>,
    last_cmd: Option<String>,
}

impl State {
    fn reset() -> Self {
        Self {
            echo: true,
            headers: false,
            spaces: true,
            linefeeds: false,
            sh: 0x7DF,
            cra: None,
            last_cmd: None,
        }
    }
}

/// Start an emulator; returns the host side of the pipe and the command log.
pub fn spawn(cfg: EmuConfig, ecus: Vec<Ecu>) -> (DuplexStream, CommandLog) {
    let (host, dev) = tokio::io::duplex(64 * 1024);
    let log: CommandLog = Default::default();
    tokio::spawn(run(dev, cfg, ecus, log.clone()));
    (host, log)
}

async fn run(mut io: DuplexStream, cfg: EmuConfig, ecus: Vec<Ecu>, log: CommandLog) {
    let mut st = State::reset();
    if !cfg.stale_on_connect.is_empty() && io.write_all(&cfg.stale_on_connect).await.is_err() {
        return;
    }
    let mut line = String::new();
    let mut resetting_until: Option<Instant> = None;
    let mut byte = [0u8; 1];
    let mut first_reply = true;
    let mut drop_first = cfg.drop_first.clone();

    loop {
        // Finish a pending ATZ reset: print the banner and restore defaults.
        if let Some(until) = resetting_until {
            let now = Instant::now();
            if now >= until {
                resetting_until = None;
                line.clear();
                st = State::reset();
                let out = format!("\r\r{}\r\r>", cfg.banner);
                if io.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                continue;
            }
            match timeout(until - now, io.read(&mut byte)).await {
                Err(_) => continue,
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(_)) => {
                    // Input during reset is lost; record whole lines for tests.
                    if byte[0] == b'\r' {
                        log.lock().unwrap().push((std::mem::take(&mut line), false));
                    } else if byte[0] != b'\n' {
                        line.push(byte[0] as char);
                    }
                    continue;
                }
            }
        }

        match io.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        match byte[0] {
            b'\r' => {}
            b'\n' => continue,
            b => {
                line.push(b as char);
                continue;
            }
        }

        let typed = std::mem::take(&mut line);
        let cmd: String = typed
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();
        if drop_first
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case(&cmd))
        {
            drop_first = None;
            log.lock().unwrap().push((typed, false));
            continue;
        }
        log.lock().unwrap().push((typed.clone(), true));

        let mut latency = cfg.reply_latency_ms;
        if first_reply {
            latency += cfg.first_reply_latency_ms;
            first_reply = false;
        }
        if let Some((_, ms)) = cfg
            .slow_commands
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(&cmd))
        {
            latency += *ms;
        }
        if latency > 0 {
            sleep(Duration::from_millis(latency)).await;
        }
        let mut out = String::new();
        if st.echo {
            out.push_str(&typed);
            out.push('\r');
            if st.linefeeds {
                out.push('\n');
            }
        }

        let cmd = if cmd.is_empty() {
            match st.last_cmd.clone() {
                Some(c) => c,
                None => {
                    out.push('?');
                    out.push_str(&end(&st));
                    if io.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                    continue;
                }
            }
        } else {
            cmd
        };
        st.last_cmd = Some(cmd.clone());

        if cmd == "ATZ" || cmd == "ATWS" {
            if io.write_all(out.as_bytes()).await.is_err() {
                return;
            }
            resetting_until = Some(Instant::now() + Duration::from_millis(cfg.reset_ms));
            continue;
        }

        if cmd == "ATMA" {
            if io.write_all(out.as_bytes()).await.is_err() {
                return;
            }
            if !monitor(&mut io, &cfg, &st).await {
                return;
            }
            continue;
        }

        let is_obd = !cmd.starts_with("AT") && is_hex_request(&cmd);
        let body: Vec<String> = match at_command(&cmd, &mut st, &cfg) {
            Some(r) => vec![r],
            None if is_obd => obd_request(&cmd, &st, &ecus),
            None => vec!["?".into()],
        };

        if io.write_all(out.as_bytes()).await.is_err() {
            return;
        }
        let eol = if st.linefeeds { "\r\n" } else { "\r" };
        let mut interrupted = false;
        for (i, l) in body.iter().enumerate() {
            if i > 0 && cfg.line_delay_ms > 0 {
                match pause(&mut io, &cfg, cfg.line_delay_ms).await {
                    Pause::Closed => return,
                    Pause::Interrupted => {
                        interrupted = true;
                        break;
                    }
                    Pause::Elapsed => {}
                }
            }
            if io.write_all(format!("{l}{eol}").as_bytes()).await.is_err() {
                return;
            }
        }
        // After the last frame the ELM keeps listening for further replies
        // for up to the AT ST time before it prints the prompt.
        if is_obd && !interrupted && cfg.post_reply_wait_ms > 0 {
            if let Pause::Closed = pause(&mut io, &cfg, cfg.post_reply_wait_ms).await {
                return;
            }
        }
        if io.write_all(format!("{eol}>").as_bytes()).await.is_err() {
            return;
        }
    }
}

enum Pause {
    Elapsed,
    Interrupted,
    Closed,
}

/// Wait `ms` while the ELM is busy with an OBD request. With
/// `interrupt_aborts`, a received character ends the wait (the datasheet:
/// any RS232 character interrupts OBD processing). The interrupting
/// character is discarded, as the datasheet states for monitoring; the rest
/// of its line is read as the next command. Without it, input stays buffered.
async fn pause(io: &mut DuplexStream, cfg: &EmuConfig, ms: u64) -> Pause {
    let gap = Duration::from_millis(ms);
    if !cfg.interrupt_aborts {
        sleep(gap).await;
        return Pause::Elapsed;
    }
    let mut byte = [0u8; 1];
    match timeout(gap, io.read(&mut byte)).await {
        Ok(Ok(n)) if n > 0 => Pause::Interrupted,
        Ok(_) => Pause::Closed,
        Err(_) => Pause::Elapsed,
    }
}

fn end(st: &State) -> String {
    if st.linefeeds {
        "\r\n\r\n>".into()
    } else {
        "\r\r>".into()
    }
}

fn at_command(cmd: &str, st: &mut State, cfg: &EmuConfig) -> Option<String> {
    if !cmd.starts_with("AT") {
        return None;
    }
    let arg = &cmd[2..];
    let ok = Some("OK".to_string());
    match arg {
        "E0" => {
            st.echo = false;
            ok
        }
        "E1" => {
            st.echo = true;
            ok
        }
        "H0" => {
            st.headers = false;
            ok
        }
        "H1" => {
            st.headers = true;
            ok
        }
        "S0" => {
            st.spaces = false;
            ok
        }
        "S1" => {
            st.spaces = true;
            ok
        }
        "L0" => {
            st.linefeeds = false;
            ok
        }
        "L1" => {
            st.linefeeds = true;
            ok
        }
        "I" => Some(cfg.banner.clone()),
        "CRA" => {
            st.cra = None;
            ok
        }
        _ if arg.starts_with("CRA") => parse_header(&arg[3..])
            .map(|h| {
                st.cra = Some(h);
                "OK".to_string()
            })
            .or(Some("?".into())),
        _ if arg.starts_with("SH") => parse_header(&arg[2..])
            .map(|h| {
                st.sh = h;
                "OK".to_string()
            })
            .or(Some("?".into())),
        _ if arg.starts_with("FCSH") => parse_header(&arg[4..])
            .map(|_| "OK".to_string())
            .or(Some("?".into())),
        _ if ["AL", "AT0", "AT1", "AT2", "CAF0", "CAF1", "D"].contains(&arg)
            || arg.starts_with("ST")
            || arg.starts_with("CP")
            || arg.starts_with("FCSD")
            || arg.starts_with("FCSM")
            || arg.starts_with("SP") =>
        {
            ok
        }
        _ => Some("?".into()),
    }
}

/// ATSH/ATCRA accept 3, 6 or 8 hex digits.
fn parse_header(s: &str) -> Option<u32> {
    if ![3, 6, 8].contains(&s.len()) {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

fn is_hex_request(cmd: &str) -> bool {
    !cmd.is_empty() && cmd.len().is_multiple_of(2) && cmd.chars().all(|c| c.is_ascii_hexdigit())
}

fn obd_request(cmd: &str, st: &State, ecus: &[Ecu]) -> Vec<String> {
    let mut lines = Vec::new();
    for ecu in ecus {
        let addressed = ecu.tx == st.sh || (st.sh == 0x7DF && ecu.functional);
        if !addressed {
            continue;
        }
        if let Some(cra) = st.cra {
            if cra != ecu.rx {
                continue;
            }
        }
        if let Some(frames) = ecu.responses.get(cmd) {
            lines.extend(render(ecu.rx, frames, st));
        }
    }
    if lines.is_empty() {
        lines.push("NO DATA".into());
    }
    lines
}

fn fmt_bytes(b: &[u8], spaces: bool) -> String {
    let parts: Vec<String> = b.iter().map(|x| format!("{x:02X}")).collect();
    parts.join(if spaces { " " } else { "" })
}

/// Print ECU frames as the ELM327 does with CAN auto-formatting on.
fn render(rx: u32, frames: &[Vec<u8>], st: &State) -> Vec<String> {
    let sep = if st.spaces { " " } else { "" };
    if st.headers {
        return frames
            .iter()
            .map(|f| format!("{rx:03X}{sep}{}", fmt_bytes(f, st.spaces)))
            .collect();
    }
    let mut out = Vec::new();
    let Some(first) = frames.first() else {
        return out;
    };
    match first[0] >> 4 {
        0 => {
            let n = (first[0] & 0x0F) as usize;
            out.push(fmt_bytes(&first[1..(1 + n).min(first.len())], st.spaces));
        }
        1 => {
            let total = (((first[0] & 0x0F) as usize) << 8) | first[1] as usize;
            out.push(format!("{total:03X}"));
            out.push(format!("0:{sep}{}", fmt_bytes(&first[2..], st.spaces)));
            for f in &frames[1..] {
                out.push(format!(
                    "{:X}:{sep}{}",
                    f[0] & 0x0F,
                    fmt_bytes(&f[1..], st.spaces)
                ));
            }
        }
        _ => out.push(fmt_bytes(first, st.spaces)),
    }
    out
}

/// ATMA: print broadcast frames until any character arrives.
async fn monitor(io: &mut DuplexStream, cfg: &EmuConfig, st: &State) -> bool {
    let mut byte = [0u8; 1];
    let eol = if st.linefeeds { "\r\n" } else { "\r" };
    let sep = if st.spaces { " " } else { "" };
    let line = |i: usize| {
        let (id, data) = &cfg.broadcast[i % cfg.broadcast.len()];
        if st.headers {
            format!("{id:03X}{sep}{}{eol}", fmt_bytes(data, st.spaces))
        } else {
            format!("{}{eol}", fmt_bytes(data, st.spaces))
        }
    };
    let mut i = 0usize;
    loop {
        if cfg.monitor_close_after.is_some_and(|n| i >= n) {
            return false;
        }
        if cfg.monitor_self_stop_after.is_some_and(|n| i >= n) {
            return io
                .write_all(format!("BUFFER FULL{eol}{eol}>").as_bytes())
                .await
                .is_ok();
        }
        if let Ok(r) = timeout(Duration::from_millis(20), io.read(&mut byte)).await {
            return match r {
                Ok(n) if n > 0 => {
                    if cfg.monitor_stop_delay_ms > 0 {
                        if !cfg.broadcast.is_empty()
                            && io.write_all(line(i).as_bytes()).await.is_err()
                        {
                            return false;
                        }
                        sleep(Duration::from_millis(cfg.monitor_stop_delay_ms)).await;
                    }
                    io.write_all(format!("{eol}>").as_bytes()).await.is_ok()
                }
                _ => false,
            };
        }
        if cfg.broadcast.is_empty() {
            i += 1;
            continue;
        }
        let l = line(i);
        i += 1;
        if io.write_all(l.as_bytes()).await.is_err() {
            return false;
        }
    }
}

/// Panic when a CAN frame is shorter than its ISO-TP PCI byte announces.
fn check_pci(request: &str, f: &[u8]) {
    let need = match f.first().map(|b| b >> 4) {
        None => 1,
        Some(0) => 1 + (f[0] & 0x0F) as usize,
        Some(1) => 2,
        Some(_) => 1, // consecutive and flow-control frames: PCI byte only
    };
    assert!(
        f.len() >= need,
        "response to {request}: frame {} is {} byte(s), its PCI needs at least {need}",
        fmt_bytes(f, true),
        f.len()
    );
}

pub fn hex(s: &str) -> Vec<u8> {
    if let Some(c) = s.chars().find(|c| !c.is_ascii_hexdigit() && *c != ' ') {
        panic!("hex: unexpected character {c:?} in {s:?}");
    }
    let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    assert!(
        s.len().is_multiple_of(2),
        "hex: odd number of digits in {s:?}"
    );
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Split a payload into ISO-TP frames (padding 0xAA).
pub fn isotp_frames(payload: &[u8]) -> Vec<Vec<u8>> {
    assert!(
        payload.len() <= 0xFFF,
        "isotp_frames: payload of {} bytes exceeds 0xFFF",
        payload.len()
    );
    let pad = |mut f: Vec<u8>| {
        f.resize(8, 0xAA);
        f
    };
    if payload.len() <= 7 {
        let mut f = vec![payload.len() as u8];
        f.extend_from_slice(payload);
        return vec![pad(f)];
    }
    let mut frames = Vec::new();
    let mut f = vec![
        0x10 | ((payload.len() >> 8) as u8 & 0x0F),
        payload.len() as u8,
    ];
    f.extend_from_slice(&payload[..6]);
    frames.push(f);
    for (i, chunk) in payload[6..].chunks(7).enumerate() {
        let mut f = vec![0x20 | (((i + 1) & 0x0F) as u8)];
        f.extend_from_slice(chunk);
        frames.push(pad(f));
    }
    frames
}
