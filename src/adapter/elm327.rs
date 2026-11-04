// ELM327 session helper, generic over an async byte stream
// (T: AsyncRead + AsyncWrite + Unpin) so it can drive both Bluetooth RFCOMM
// and USB serial transports.

use super::{classify, AdapterError, Metrics};
use crate::profile::{BroadcastSource, FieldSpec, Source, UdsPidSource, VehicleProfile};
use anyhow::{anyhow, Context, Result};
use log::{debug, trace, warn};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::io::{Error as IoError, ErrorKind};
use tokio::time::timeout;

/// Default ELM327 initialisation sequence. A profile may override it via
/// elm327.init.
pub const DEFAULT_INIT: &[&str] = &[
    "ATZ", "ATE0", "ATAL", "ATST96", "ATCP18", "ATFCSD300000", "ATSP6",
];

const EOM_PROMPT: u8 = b'>';
const SEND_CMD_TIMEOUT: Duration = Duration::from_millis(5000);
const RESYNC_TIMEOUT: Duration = Duration::from_secs(2);
/// Line cap for one multi-line reply: a length line, a first line of 6 bytes
/// and 7-byte lines up to the 0xFFF-byte ISO-TP maximum, with margin.
const MAX_REPLY_LINES: usize = 600;
/// Overall time allowed for one multi-line reply, from the request write.
const REPLY_DEADLINE: Duration = Duration::from_secs(10);
const REPLY_LINE_TIMEOUT: Duration = Duration::from_millis(500);

/// ATSH, ATCRA and ATFCSH take 3, 6 or 8 hex digits.
fn check_can_id(cmd: &str, id: &str) -> Result<()> {
    if !crate::profile::is_valid_can_id(id) {
        return Err(anyhow!("{cmd} id '{id}' must be 3, 6 or 8 hex digits"));
    }
    Ok(())
}

/// One session owns one buffered reader for its whole life: bytes that arrive
/// after a '>' prompt stay in the buffer for the next read instead of being
/// dropped with a per-call reader. Writes pass through to the inner stream.
pub struct Elm327Session<T> {
    pub stream: BufReader<T>,
    /// Set when a command timed out, so its late reply is still in flight.
    /// The next `send_cmd` reads up to the prompt before writing.
    out_of_sync: bool,
}

impl<T> Elm327Session<T>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    pub fn new(stream: T) -> Self {
        Self { stream: BufReader::new(stream), out_of_sync: false }
    }

    /// After a timed-out command its late reply is still in flight: read up to
    /// the prompt before anything is written, and clear the flag only when the
    /// prompt was found.
    async fn resync(&mut self) -> Result<()> {
        if self.out_of_sync {
            if !self.read_until_prompt(RESYNC_TIMEOUT).await {
                return Err(anyhow::Error::new(IoError::new(
                    ErrorKind::TimedOut,
                    "resync: no prompt after timed-out command",
                )));
            }
            self.out_of_sync = false;
        }
        Ok(())
    }

    /// Send a command, read until the '>' prompt, return the raw bytes.
    pub async fn send_cmd(&mut self, cmd: &str) -> Result<Vec<u8>> {
        self.resync().await?;
        let mut wire = Vec::with_capacity(cmd.len() + 1);
        wire.extend_from_slice(cmd.as_bytes());
        wire.push(b'\r');
        debug!("write: {}", String::from_utf8_lossy(&wire));

        if let Err(e) = self.stream.write_all(&wire).await {
            return Err(anyhow::Error::new(IoError::new(e.kind(), format!("write '{cmd}': {e}"))));
        }

        let mut buf = Vec::new();

        match timeout(SEND_CMD_TIMEOUT, self.stream.read_until(EOM_PROMPT, &mut buf)).await {
            Ok(Ok(0)) => Err(anyhow::Error::new(IoError::new(
                ErrorKind::UnexpectedEof,
                format!("0 bytes for '{cmd}'"),
            ))),
            Ok(Ok(_)) if buf.last() != Some(&EOM_PROMPT) => Err(anyhow::Error::new(IoError::new(
                ErrorKind::UnexpectedEof,
                format!("link closed mid-reply to '{cmd}'"),
            ))),
            Ok(Ok(_)) => {
                drop_nulls(&mut buf);
                strip_echo(&mut buf, cmd);
                let ascii = String::from_utf8_lossy(&buf);
                trace!("Response ASCII: {ascii}");
                if ascii.contains("NO DATA") {
                    return Err(anyhow::Error::new(IoError::other("no data")));
                }
                Ok(buf)
            }
            Ok(Err(e)) => Err(anyhow::Error::new(IoError::new(e.kind(), format!("read '{cmd}': {e}")))),
            Err(_) => {
                self.out_of_sync = true;
                Err(anyhow::Error::new(IoError::new(ErrorKind::TimedOut, format!("timeout '{cmd}'"))))
            }
        }
    }

    /// Discard buffered and incoming bytes until none arrives for `quiet_ms`
    /// or `max_ms` (capped at 2000) elapse, so a stale reply cannot be read as
    /// the answer to the next command.
    pub async fn drain_input(&mut self, quiet_ms: u64, max_ms: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(max_ms.min(2000));
        let quiet = Duration::from_millis(quiet_ms);
        let pending = self.stream.buffer().len();
        if pending > 0 {
            self.stream.consume(pending);
            trace!("drained {pending} buffered bytes");
        }
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() { break; }
            let n = match timeout(quiet.min(left), self.stream.fill_buf()).await {
                Ok(Ok(b)) => b.len(),
                _ => break,
            };
            if n == 0 { break; }
            self.stream.consume(n);
            trace!("drained {n} bytes");
        }
    }

    /// Read through the first '>' prompt, or until `max` elapses. Bytes after
    /// the prompt stay buffered. Returns whether the prompt was found.
    async fn read_until_prompt(&mut self, max: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + max;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() { break; }
            let (used, found) = match timeout(left, self.stream.fill_buf()).await {
                Ok(Ok(b)) if !b.is_empty() => match b.iter().position(|c| *c == EOM_PROMPT) {
                    Some(i) => (i + 1, true),
                    None => (b.len(), false),
                },
                _ => break,
            };
            self.stream.consume(used);
            if found { return true; }
        }
        false
    }

    /// Read a single line (terminated by \r or \n) with a per-read timeout.
    /// Returns the line bytes excluding the terminator; bytes after it stay
    /// buffered. A timeout with nothing read is an error.
    async fn read_line(&mut self, millis: u64) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let (used, done) = match timeout(Duration::from_millis(millis), self.stream.fill_buf()).await {
                Ok(Ok([])) => break,
                Ok(Ok(b)) => {
                    let mut used = 0;
                    let mut done = false;
                    for &c in b {
                        used += 1;
                        if c == b'\r' || c == b'\n' {
                            if !out.is_empty() { done = true; break; }
                        } else {
                            out.push(c);
                            if out == [EOM_PROMPT] { done = true; break; }
                        }
                    }
                    (used, done)
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    if out.is_empty() {
                        return Err(std::io::Error::new(ErrorKind::TimedOut, "line read timeout"));
                    }
                    break;
                }
            };
            self.stream.consume(used);
            if done { break; }
        }
        Ok(out)
    }

    /// Send ATE0 and require "OK". A missing reply or one without "OK" (the
    /// chip drops input while it resets) is retried once after a drain.
    async fn send_ate0(&mut self, cmd: &str) -> Result<()> {
        let acked = |r: &Result<Vec<u8>>| {
            r.as_ref().is_ok_and(|b| String::from_utf8_lossy(b).contains("OK"))
        };
        if acked(&self.send_cmd(cmd).await) {
            return Ok(());
        }
        self.drain_input(200, 1000).await;
        // The chip drops input while it resets, so the lost command has no
        // reply to wait for; the drain above is the resync.
        self.out_of_sync = false;
        let retry = self.send_cmd(cmd).await;
        if acked(&retry) {
            return Ok(());
        }
        match retry {
            Err(e) => Err(e.context(format!("init step '{cmd}' failed after retry"))),
            Ok(_) => Err(anyhow!("init step '{cmd}' failed: no OK after retry")),
        }
    }

    /// Run a sequence of ELM327 init commands. Each failure is fatal. Input
    /// left over from the link probe or a previous session is discarded
    /// first. After ATZ the reply must carry a banner (the chip is deaf while
    /// it resets); a bare '?' prompt is a stale reply, so the next prompt is
    /// awaited. ATE0 must be acknowledged, and is appended when the list has
    /// none, since echo left on shifts every later reply.
    pub async fn run_init(&mut self, init: &[&str]) -> Result<()> {
        self.drain_input(200, 1500).await;
        for cmd in init {
            if cmd.trim().eq_ignore_ascii_case("ATE0") {
                self.send_ate0(cmd).await?;
                continue;
            }
            let reply = self.send_cmd(cmd).await
                .with_context(|| format!("init step '{cmd}' failed"))?;
            if cmd.trim().eq_ignore_ascii_case("ATZ") {
                let no_banner = String::from_utf8_lossy(&reply)
                    .replace(['?', '>'], "")
                    .trim()
                    .is_empty();
                if no_banner {
                    self.read_until_prompt(Duration::from_secs(3)).await;
                }
                self.drain_input(200, 1000).await;
            }
        }
        if !init.iter().any(|c| c.trim().eq_ignore_ascii_case("ATE0")) {
            self.send_ate0("ATE0").await?;
        }
        Ok(())
    }

    /// Send a uds_pid source and return the single-frame response payload.
    pub async fn poll_uds_pid(&mut self, uds: &UdsPidSource) -> Result<Vec<u8>> {
        check_can_id("ATSH", &uds.ecu_tx)?;
        check_can_id("ATCRA", &uds.ecu_rx)?;
        self.send_header_cmd(&format!("ATSH{}", uds.ecu_tx)).await?;
        self.send_header_cmd(&format!("ATCRA{}", uds.ecu_rx)).await?;
        self.send_header_cmd(&format!("ATFCSH{}", uds.ecu_tx)).await?;
        self.send_cmd("ATFCSD300000").await?;
        self.send_cmd("ATFCSM1").await?;
        // Single-frame parsing expects replies without the CAN-ID prefix.
        self.send_cmd("ATH0").await?;
        if let Some(pre) = &uds.pre_request {
            // Pre-request errors are intentionally ignored — some ECUs don't ACK.
            let _ = self.send_cmd(pre).await;
        }
        let raw = self.send_cmd(&uds.pid).await?;
        let payload = get_payload(&String::from_utf8_lossy(&raw));
        let service = uds.pid.get(..2).and_then(|s| u8::from_str_radix(s, 16).ok());
        if let (Some(&0x7F), Some(&svc)) = (payload.first(), payload.get(1)) {
            if service.is_none() || service == Some(svc) {
                let nrc = payload.get(2).map_or_else(|| "none".to_string(), |n| format!("{n:02X}"));
                return Err(anyhow::Error::new(std::io::Error::other(format!(
                    "ECU rejected request {}: negative response code {nrc}",
                    uds.pid
                ))));
            }
        }
        Ok(payload)
    }

    /// Send a header command; a "?" reply means the adapter rejected it.
    async fn send_header_cmd(&mut self, cmd: &str) -> Result<()> {
        let reply = self.send_cmd(cmd).await?;
        if String::from_utf8_lossy(&reply).contains('?') {
            return Err(anyhow!("adapter rejected {cmd}"));
        }
        Ok(())
    }

    /// Poll a uds_pid source with ISO-TP flow-control reassembly, for responses
    /// that span multiple frames. Used when the source sets multiframe = true.
    pub async fn poll_uds_pid_multiframe(&mut self, uds: &UdsPidSource) -> Result<Vec<u8>> {
        check_can_id("ATSH", &uds.ecu_tx)?;
        check_can_id("ATCRA", &uds.ecu_rx)?;

        self.send_header_cmd(&format!("ATSH{}", uds.ecu_tx)).await?;
        self.send_header_cmd(&format!("ATCRA{}", uds.ecu_rx)).await?;
        self.send_header_cmd(&format!("ATFCSH{}", uds.ecu_tx)).await?;
        self.send_cmd("ATFCSD300000").await?;
        self.send_cmd("ATFCSM1").await?;
        self.send_cmd("ATH0").await?;

        // Send the PID directly (then read lines rather than wait for a prompt).
        self.resync().await?;
        let cmd = format!("{}\r", uds.pid);
        self.stream.write_all(cmd.as_bytes()).await
            .context("multiframe PID write")?;

        let service = uds.pid.get(..2).and_then(|s| u8::from_str_radix(s, 16).ok());

        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut saw_prompt = false;
        let deadline = tokio::time::Instant::now() + REPLY_DEADLINE;

        for _ in 0..MAX_REPLY_LINES {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() { break; }
            let wait = REPLY_LINE_TIMEOUT.min(remaining);
            let line = match self.read_line(wait.as_millis().max(1) as u64).await {
                Ok(l) => l,
                Err(e) if e.kind() == ErrorKind::TimedOut => break,
                Err(e) => {
                    let _ = self.send_cmd("ATH0").await;
                    return Err(anyhow::Error::new(e).context("multiframe read"));
                }
            };
            // read_line skips empty lines, so an empty result is end-of-file.
            if line.is_empty() {
                return Err(anyhow::Error::new(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "link closed",
                )).context("multiframe read"));
            }
            let trimmed = String::from_utf8_lossy(&line).trim().to_string();
            if trimmed == ">" {
                saw_prompt = true;
                break;
            }
            if trimmed.is_empty() { break; }
            if trimmed.contains("NO DATA") || is_negative_response(&trimmed, service) {
                // The line ended at its CR; consume the reply's prompt so
                // ATH0 reads its own reply.
                self.read_until_prompt(Duration::from_secs(2)).await;
                let _ = self.send_cmd("ATH0").await;
                return Err(anyhow!("UDS error response: {trimmed}"));
            }
            // The ELM327 sends the ISO-TP flow-control frame itself
            // (ATFCSM1 with ATFCSH/ATFCSD above). Anything written to the
            // serial link here would be read as typed input and interrupt
            // the request.
            lines.push(line);
        }

        // A read timeout or the line cap leaves the rest of the reply and its
        // prompt unread; consume them so ATH0 reads its own reply.
        if !saw_prompt {
            self.read_until_prompt(Duration::from_secs(2)).await;
        }

        // Restore the headers-OFF baseline that single-frame get_payload
        // expects (DEFAULT_INIT relies on the ELM327 power-on default of
        // headers off; ATH0 here prevents header bytes leaking into the next
        // cycle's single-frame parsing).
        let _ = self.send_cmd("ATH0").await;
        let _ = self.send_cmd("ATS1").await;
        let _ = self.send_cmd("ATCRA").await;

        // Return the header-inclusive payload — same convention as the
        // single-frame poll_uds_pid (get_payload also keeps the header). The
        // extract_value byte branch applies its +3 header skip consistently.
        assemble_iso_tp(&lines).ok_or_else(|| anyhow!("multiframe assembly failed"))
    }

    /// Run a BroadcastSource: send init, send the command (typically ATMA),
    /// read lines until the deadline or stop_when is satisfied, route to frames,
    /// then extract each frame's fields from its most recent payload.
    pub async fn monitor_broadcast(
        &mut self,
        spec: &BroadcastSource,
    ) -> Result<HashMap<String, f32>> {
        for cmd in &spec.init {
            if let Err(e) = self.send_cmd(cmd).await {
                warn!("broadcast init '{cmd}' failed: {e:#}");
                let timed_out = e.chain()
                    .filter_map(|c| c.downcast_ref::<std::io::Error>())
                    .any(|io| io.kind() == ErrorKind::TimedOut);
                if timed_out {
                    return Err(e.context(format!("broadcast init '{cmd}'")));
                }
            }
        }
        // ATMA streams continuously; do not wait for a '>' prompt.
        self.resync().await?;
        self.stream.write_all(format!("{}\r", spec.command).as_bytes()).await
            .context("broadcast command write")?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(spec.deadline_ms);
        let mut lines: Vec<String> = Vec::new();
        let mut consecutive_timeouts: u64 = 0;
        let idle_attempts = spec.idle_timeout_ms.div_ceil(200).max(1);

        // CAN-IDs whose frame carries a field listed in stop_when. Once a line
        // has arrived for each of them, the wanted data is in hand and the scan
        // can end before the deadline.
        let needed_ids: HashSet<String> = spec.frames.iter()
            .filter(|f| f.fields.iter().any(|fld| spec.stop_when.iter().any(|s| s == &fld.name)))
            .map(|f| f.can_id.clone())
            .collect();
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut prompt_seen = false;

        while tokio::time::Instant::now() < deadline {
            match self.read_line(200).await {
                Ok(b) => {
                    if b.is_empty() {
                        return Err(anyhow::Error::new(std::io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "broadcast stream closed",
                        )));
                    }
                    consecutive_timeouts = 0;
                    let ascii = String::from_utf8_lossy(&b).into_owned();
                    if ascii.contains('>') {
                        prompt_seen = true;
                        break;
                    }
                    if !ascii.trim().is_empty() {
                        if let Some(id) = ascii.split_whitespace().next() {
                            if id.chars().all(|c| c.is_ascii_hexdigit()) {
                                seen_ids.insert(id.to_string());
                            }
                        }
                        lines.push(ascii);
                        if !needed_ids.is_empty() && needed_ids.iter().all(|id| seen_ids.contains(id)) {
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::TimedOut => {
                    consecutive_timeouts += 1;
                    if consecutive_timeouts >= idle_attempts { break; }
                }
                Err(e) => return Err(anyhow::Error::new(e).context("broadcast read")),
            }
        }

        // Stop ATMA cleanly: send a CR, then read until the '>' prompt. The
        // ELM completes the line it is printing first, so the wait is bounded
        // overall rather than per byte. When the ELM already left monitoring
        // and printed its prompt, a CR would repeat the last command (ATMA).
        if !prompt_seen {
            let _ = self.stream.write_all(b"\r").await;
            self.read_until_prompt(Duration::from_secs(2)).await;
        }
        let _ = self.send_cmd("ATCRA").await;
        // Restore headers OFF: the broadcast init turned headers ON for ATMA,
        // but single-frame get_payload on the next cycle expects them off.
        let _ = self.send_cmd("ATH0").await;

        // Extract from the most recent payload seen for each CAN-ID.
        let mut metrics: HashMap<String, f32> = HashMap::new();
        let routed = route_broadcast_lines(&lines);
        for frame in &spec.frames {
            if let Some(payloads) = routed.get(&frame.can_id) {
                if let Some(latest) = payloads.last() {
                    for fld in frame.fields.iter().filter(|f| f.enabled) {
                        if let Some(v) = extract_value_broadcast(latest, fld) {
                            metrics.insert(fld.name.clone(), v);
                        }
                    }
                }
            }
        }
        Ok(metrics)
    }
}

/// Poll every source of a profile once and merge the results. Errors go
/// through `classify`: a lost link or a down CAN bus ends the cycle at once,
/// any other failure is logged and the next source is tried.
pub async fn poll_sources<T>(
    session: &mut Elm327Session<T>,
    profile: &VehicleProfile,
) -> std::result::Result<Metrics, AdapterError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut metrics = Metrics::new();
    for source in &profile.sources {
        match source {
            Source::UdsPid(uds) => {
                debug!("Polling PID {} (multiframe={})", uds.pid, uds.multiframe);
                let result = if uds.multiframe {
                    session.poll_uds_pid_multiframe(uds).await
                } else {
                    session.poll_uds_pid(uds).await
                };
                match result {
                    Ok(payload) if payload.is_empty() => continue,
                    Ok(payload) => {
                        for f in uds.fields.iter().filter(|f| f.enabled) {
                            if let Some(v) = extract_value(&payload, f) {
                                debug!("{}: {}", f.name, v);
                                metrics.insert(f.name.clone(), v);
                            } else {
                                warn!("extract failed for field '{}'", f.name);
                            }
                        }
                    }
                    Err(e) => match classify(e) {
                        AdapterError::Transient(e) => warn!("PID {} failed: {e:#}", uds.pid),
                        other => return Err(other),
                    },
                }
            }
            Source::Broadcast(bc) => match session.monitor_broadcast(bc).await {
                Ok(m) => metrics.extend(m),
                Err(e) => match classify(e) {
                    AdapterError::Transient(e) => warn!("broadcast source failed: {e:#}"),
                    other => return Err(other),
                },
            },
        }
    }
    Ok(metrics)
}

/// Remove NULL bytes, which the ELM327 may insert into its output.
fn drop_nulls(buf: &mut Vec<u8>) {
    buf.retain(|b| *b != 0);
}

/// Drop the first non-empty line of `buf` when it is the command just sent
/// (echo on), comparing case-insensitively and ignoring spaces.
fn strip_echo(buf: &mut Vec<u8>, cmd: &str) {
    let norm = |b: &[u8]| -> String {
        String::from_utf8_lossy(b).chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase()
    };
    let Some(start) = buf.iter().position(|b| !matches!(b, b'\r' | b'\n')) else { return };
    let end = buf[start..].iter()
        .position(|b| matches!(b, b'\r' | b'\n'))
        .map_or(buf.len(), |p| start + p);
    if norm(&buf[start..end]) == norm(cmd.as_bytes()) {
        buf.drain(..end);
    }
}

/// Hex bytes of one reply line. A line with spaces is read token by token and
/// only 2-digit tokens count, so a 3-digit CAN-ID cannot shift the pairing; a
/// line without spaces (ATS0) is read as consecutive hex pairs.
// as_chunks needs Rust 1.88; production code targets 1.80; unknown_lints keeps older clippy quiet
#[allow(clippy::chunks_exact_to_as_chunks, unknown_lints)]
fn hex_bytes(data: &str) -> Vec<u8> {
    if data.contains(' ') {
        data.split_whitespace()
            .filter(|t| t.len() == 2)
            .filter_map(|t| u8::from_str_radix(t, 16).ok())
            .collect()
    } else {
        data.as_bytes().chunks_exact(2)
            .filter_map(|p| std::str::from_utf8(p).ok())
            .filter_map(|p| u8::from_str_radix(p, 16).ok())
            .collect()
    }
}

/// True when a reply line is a UDS negative response: 7F, then the requested
/// service id, then the response code ("7F 21 12", or "7F2112" with ATS0). When
/// the request has no parsable service id, 7F followed by any second byte
/// qualifies. A negative response is always a single frame, so a line carrying an "N:"
/// multi-frame index is data and never matches, whatever bytes it holds.
// MSRV: avoids APIs newer than Rust 1.80
#[allow(clippy::unnecessary_map_or)]
fn is_negative_response(line: &str, service: Option<u8>) -> bool {
    if line.contains(':') { return false; }
    matches!(hex_bytes(line.trim()).get(..2), Some([0x7F, s]) if service.map_or(true, |sv| sv == *s))
}

/// Strip ELM327 framing from a raw response string and return the UDS payload
/// bytes (header + data). Lines with spaces are read token by token and only
/// 2-digit tokens count as bytes, so a 3-digit CAN-ID printed with headers on
/// (e.g. "7EC 06 62 20 06 02 15 04") cannot shift the character pairing; the
/// PCI byte after the CAN-ID is still part of the payload, so byte indices are
/// only correct when headers are off (poll_uds_pid sends ATH0). Lines without
/// spaces (ATS0, e.g. "0:620105FFFB74") are read as consecutive hex pairs.
pub fn get_payload(response: &str) -> Vec<u8> {
    let frames: Vec<&str> = response.split(['\r', '\n'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| !s.contains("SEARCHING"))
        .collect();

    let mut payload = Vec::new();
    let mut is_first = true;

    for frame in frames {
        let compact = frame.replace(' ', "");
        if is_first && compact.len() <= 3 {
            is_first = false;
            continue;
        }
        is_first = false;

        let data = frame.split_once(':').map_or(frame, |(_, rest)| rest.trim());
        payload.extend(hex_bytes(data));
    }
    payload
}

/// Extract a packed value using Motorola/big-endian bit numbering — bit 0 is
/// the MSB of byte 0. bit_length is 1..=16. Reads contiguously across bytes.
fn extract_bits(payload: &[u8], bit_offset: u32, bit_length: u32) -> Option<u32> {
    if bit_length == 0 || bit_length > 16 { return None; }
    let last_bit = bit_offset.checked_add(bit_length)?;
    let needed_bytes = last_bit.div_ceil(8) as usize;
    if needed_bytes > payload.len() { return None; }

    let mut acc: u32 = 0;
    for i in 0..bit_length {
        let abs_bit = bit_offset + i;
        let byte = payload[(abs_bit / 8) as usize];
        let bit_in_byte = 7 - (abs_bit % 8); // bit 0 = MSB
        let bit = (byte >> bit_in_byte) & 1;
        acc = (acc << 1) | bit as u32;
    }
    Some(acc)
}

/// Extract an f32 from a UDS payload according to a FieldSpec. Bit-aligned
/// extraction is tried first (when bit_offset + bit_length are set); otherwise
/// falls back to byte-aligned extraction.
pub fn extract_value(payload: &[u8], field: &FieldSpec) -> Option<f32> {
    // Bit branch first — explicit opt-in via bit_offset+bit_length. The offset
    // is shifted by 24 bits to skip the 3-byte UDS positive-response header, so
    // bit_offset is header-relative exactly like byte_index in the byte branch.
    if let (Some(bit_off), Some(bit_len)) = (field.bit_offset, field.bit_length) {
        return extract_bits(payload, bit_off.checked_add(24)?, bit_len)
            .map(|v| (v as f32) * field.multiplier + field.offset);
    }

    let byte_index = field.byte_index?;
    let length     = field.length?;
    let idx = if byte_index < 0 {
        let positive = payload.len() as i32 + byte_index;
        if positive < 0 { return None; }
        positive as usize
    } else {
        // +3 skips the UDS positive-response header (e.g. 62 01 05).
        byte_index.checked_add(3)? as usize
    };
    if idx.checked_add(length)? > payload.len() { return None; }

    let raw = if field.signed.unwrap_or(false) {
        match length {
            1 => payload[idx] as i8 as f32,
            2 => i16::from_be_bytes([payload[idx], payload[idx + 1]]) as f32,
            3 => {
                let mut v = ((payload[idx] as u32) << 16)
                          | ((payload[idx + 1] as u32) << 8)
                          |  (payload[idx + 2] as u32);
                if v & 0x800000 != 0 { v |= 0xFF000000; }
                v as i32 as f32
            }
            _ => return None,
        }
    } else {
        match length {
            1 => payload[idx] as f32,
            2 => u16::from_be_bytes([payload[idx], payload[idx + 1]]) as f32,
            3 => (((payload[idx] as u32) << 16)
               | ((payload[idx + 1] as u32) << 8)
               |  (payload[idx + 2] as u32)) as f32,
            _ => return None,
        }
    };
    Some(raw * field.multiplier + field.offset)
}

/// Reassemble an ELM327 multi-line UDS response. With headers off and
/// automatic flow control, the adapter emits an optional total-length line
/// (three hex digits, e.g. "1A4") followed by data lines. The line counter
/// ("0:", "1:") carries no PCI byte and is split off, so every byte after it
/// is data. With spaces on a line is 2-digit tokens; with spaces off it is a
/// run of hex pairs. The reply stops at the declared length when there is one,
/// and fewer bytes than declared is an incomplete reply (None); without a
/// length line all data lines are used. The UDS response header is retained,
/// matching the single-frame path.
pub fn assemble_iso_tp(lines: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut payload: Vec<u8> = Vec::new();
    let mut expected_len: Option<usize> = None;
    let mut got_data = false;

    for line in lines {
        let ascii = String::from_utf8_lossy(line);
        let trimmed = ascii.trim();
        if !got_data && expected_len.is_none() && trimmed.len() == 3 {
            if let Ok(n) = usize::from_str_radix(trimmed, 16) {
                expected_len = Some(n);
                continue;
            }
        }
        let data = trimmed.split_once(':').map_or(trimmed, |(_, rest)| rest.trim());
        let bytes = hex_bytes(data);
        if bytes.is_empty() { continue; }
        payload.extend_from_slice(&bytes);
        got_data = true;
        if let Some(exp) = expected_len {
            if payload.len() >= exp {
                payload.truncate(exp);
                break;
            }
        }
    }
    if !got_data { return None; }
    if expected_len.is_some_and(|exp| payload.len() < exp) { return None; }
    Some(payload) // header retained; the field extractor applies its own offset
}

/// Group ATMA lines by their leading CAN-ID hex token. Lines without a
/// recognisable leading hex token (e.g. "ERROR", ">") are skipped. The CAN-ID
/// token is consumed; the remaining hex tokens become the frame's data bytes.
pub fn route_broadcast_lines(lines: &[String]) -> HashMap<String, Vec<Vec<u8>>> {
    let mut out: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        if trimmed.contains("ERROR") { continue; }

        let mut parts = trimmed.split_whitespace();
        let id = match parts.next() {
            Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()) => s.to_string(),
            _ => continue,
        };
        let bytes: Vec<u8> = parts
            .filter_map(|t| u8::from_str_radix(t, 16).ok())
            .collect();
        if bytes.is_empty() { continue; }
        out.entry(id).or_default().push(bytes);
    }
    out
}

/// Like extract_value but with raw byte offsets (broadcast CAN frames carry no
/// UDS positive-response header, so there is no +3 skip).
pub fn extract_value_broadcast(payload: &[u8], field: &FieldSpec) -> Option<f32> {
    if let (Some(off), Some(len)) = (field.bit_offset, field.bit_length) {
        return extract_bits(payload, off, len)
            .map(|v| (v as f32) * field.multiplier + field.offset);
    }
    let bi = field.byte_index?;
    let length = field.length?;
    let idx = if bi < 0 {
        let positive = payload.len() as i32 + bi;
        if positive < 0 { return None; }
        positive as usize
    } else {
        bi as usize
    };
    if idx.checked_add(length)? > payload.len() { return None; }
    let raw = if field.signed.unwrap_or(false) {
        match length {
            1 => payload[idx] as i8 as f32,
            2 => i16::from_be_bytes([payload[idx], payload[idx + 1]]) as f32,
            _ => return None,
        }
    } else {
        match length {
            1 => payload[idx] as f32,
            2 => u16::from_be_bytes([payload[idx], payload[idx + 1]]) as f32,
            _ => return None,
        }
    };
    Some(raw * field.multiplier + field.offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_unsigned_byte_with_multiplier_and_offset() {
        let mut payload = vec![0u8; 32];
        payload[8] = 134;
        let f = FieldSpec {
            name: "soc".into(), byte_index: Some(5), length: Some(1),
            bit_offset: None, bit_length: None,
            multiplier: 0.5, offset: 0.0, signed: None, enabled: true,
        };
        assert_eq!(extract_value(&payload, &f), Some(67.0));
    }

    #[test]
    fn extract_signed_negative_byte() {
        let mut payload = vec![0u8; 16];
        payload[3] = 0xF6;
        let f = FieldSpec {
            name: "delta".into(), byte_index: Some(0), length: Some(1),
            bit_offset: None, bit_length: None,
            multiplier: 1.0, offset: 0.0, signed: Some(true), enabled: true,
        };
        assert_eq!(extract_value(&payload, &f), Some(-10.0));
    }

    #[test]
    fn extract_negative_byte_index_from_end() {
        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22];
        let f = FieldSpec {
            name: "x".into(), byte_index: Some(-3), length: Some(3),
            bit_offset: None, bit_length: None,
            multiplier: 1.0, offset: 0.0, signed: None, enabled: true,
        };
        assert_eq!(extract_value(&payload, &f), Some(4386.0));
    }

    #[test]
    fn extract_out_of_bounds_returns_none() {
        let payload = vec![0u8; 4];
        let f = FieldSpec {
            name: "x".into(), byte_index: Some(10), length: Some(1),
            bit_offset: None, bit_length: None,
            multiplier: 1.0, offset: 0.0, signed: None, enabled: true,
        };
        assert_eq!(extract_value(&payload, &f), None);
    }

    #[test]
    fn get_payload_handles_single_frame_with_colon() {
        let raw = "0: 62 01 05 00 11 22\r\n>";
        let p = get_payload(raw);
        assert_eq!(p, vec![0x62, 0x01, 0x05, 0x00, 0x11, 0x22]);
    }

    #[test]
    fn get_payload_strips_length_header_first_frame() {
        let raw = "014\r0:620105112233\r\n>";
        let p = get_payload(raw);
        assert_eq!(p, vec![0x62, 0x01, 0x05, 0x11, 0x22, 0x33]);
    }

    #[test]
    fn get_payload_ignores_odd_length_tokens() {
        // Headers on: the 3-digit CAN-ID is dropped instead of shifting the pairs; the PCI byte (06) stays in the payload.
        let p = get_payload("7EC 06 62 20 06 02 15 04 \r\r>");
        assert!(p.ends_with(&[0x02, 0x15, 0x04]), "{p:02X?}");
        let p = get_payload("0: 62 01 05 00 11 22\r\r>");
        assert_eq!(p, vec![0x62, 0x01, 0x05, 0x00, 0x11, 0x22]);
    }

    #[test]
    fn negative_response_needs_7f_then_the_service_byte() {
        assert!(is_negative_response("7F 21 12", Some(0x21)));
        assert!(is_negative_response("7F2112", Some(0x21)));
        assert!(!is_negative_response("0: 61 74 7F 21", Some(0x21)));
        assert!(!is_negative_response("1: 7F 21 00", Some(0x21)));
        assert!(!is_negative_response("7F 22 12", Some(0x21)));
        assert!(is_negative_response("7F 21 12", None));
        assert!(!is_negative_response("012", Some(0x21)));
        assert!(!is_negative_response("NO DATA", Some(0x21)));
        assert!(!is_negative_response("?", Some(0x21)));
        assert!(!is_negative_response("STOPPED", Some(0x21)));
        assert!(is_negative_response("7F 21 78", Some(0x21)));
        assert!(!is_negative_response("0:6174 7F21", Some(0x21)));
    }

    #[test]
    fn drop_nulls_keeps_payload_decodable() {
        let mut buf = b"62 20\0 06 02 15 04 \r\r>".to_vec();
        drop_nulls(&mut buf);
        assert!(!buf.contains(&0));
        let p = get_payload(&String::from_utf8_lossy(&buf));
        assert!(p.ends_with(&[0x02, 0x15, 0x04]), "{p:02X?}");
    }

    // Motorola bit numbering: bit 0 = MSB of byte 0.
    //   byte 0 = 0xA5 = 1010_0101 — bits 0..7
    //   byte 1 = 0x3C = 0011_1100 — bits 8..15

    #[test]
    fn extract_single_bit_msb() {
        // bit 0 (MSB of byte 0) of 0xA5 = 1
        assert_eq!(extract_bits(&[0xA5, 0x3C, 0xFF, 0x00], 0, 1), Some(1));
    }

    #[test]
    fn extract_three_bit_value_spanning_byte_boundary() {
        // bits 6..8 of 0xA5 0x3C: byte0 last two bits = 01, byte1 first bit = 0 -> 010 = 2
        assert_eq!(extract_bits(&[0xA5, 0x3C], 6, 3), Some(2));
    }

    #[test]
    fn extract_full_byte_via_bit_field() {
        // bits 8..15 of 0xA5 0x3C = 0x3C = 60
        assert_eq!(extract_bits(&[0xA5, 0x3C], 8, 8), Some(60));
    }

    #[test]
    fn extract_bit_out_of_range_returns_none() {
        assert_eq!(extract_bits(&[0xFF, 0xFF], 20, 4), None);
    }

    #[test]
    fn extract_value_bit_offset_is_uds_header_relative() {
        // extract_value treats bit_offset as relative to the data after the
        // 3-byte UDS response header, matching byte_index in the byte branch.
        let mut p = vec![0u8; 6];
        p[3] = 0b1000_0000; // first data byte, MSB set
        let f = FieldSpec {
            name: "x".into(), byte_index: None, length: None,
            bit_offset: Some(0), bit_length: Some(1),
            multiplier: 1.0, offset: 0.0, signed: None, enabled: true,
        };
        assert_eq!(extract_value(&p, &f), Some(1.0));
    }

    #[test]
    fn assemble_iso_tp_bare_length_then_data() {
        // ELM prints total length on its own line, then reassembled data lines.
        let lines = vec![
            b"012".to_vec(),                  // length = 0x12 = 18
            b"61 74 00 01 02 03 04".to_vec(), // first data line, taken wholesale
            b"05 06 07 08 09 0A 0B".to_vec(),
            b"0C 0D 0E 0F 10 11 12".to_vec(),
        ];
        let p = assemble_iso_tp(&lines).expect("payload");
        assert_eq!(&p[0..2], &[0x61, 0x74]); // header preserved (no PCI strip / no drop)
        assert!(p.len() >= 18);
        // accumulation order sanity: line2 = idx0..6, line3 = idx7..13,
        // line4 = idx14..20, so idx14 is the first byte of line4 (0x0C).
        assert_eq!(p[14], 0x0C);
    }

    #[test]
    fn assemble_iso_tp_keeps_0x2n_data_bytes_on_continuation_lines() {
        let lines = vec![
            b"010".to_vec(),
            b"0: 61 74 11 22 33 44".to_vec(),
            b"1: 21 55 66 77 88 99 AA".to_vec(),
            b"2: 2F 00 00".to_vec(),
        ];
        let p = assemble_iso_tp(&lines).expect("payload");
        assert_eq!(p, vec![0x61, 0x74, 0x11, 0x22, 0x33, 0x44, 0x21, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0x2F, 0x00, 0x00]);
    }

    #[test]
    fn assemble_iso_tp_without_length_line_uses_all_lines() {
        let lines: Vec<Vec<u8>> = vec![
            b"0: 61 74 00 01 02 03".to_vec(),
            b"1: 04 05 06 07 08 09 0A".to_vec(),
            b"2: 0B 0C 0D 0E 0F 10 11".to_vec(),
            b"3: 12 13 14 15 16 17 18".to_vec(),
        ];
        assert_eq!(assemble_iso_tp(&lines).expect("payload").len(), 27);
    }

    #[test]
    fn extract_value_overflowing_offsets_return_none() {
        let p = vec![0u8; 8];
        let mut f = FieldSpec {
            name: "x".into(), byte_index: Some(i32::MAX - 1), length: Some(1),
            bit_offset: None, bit_length: None,
            multiplier: 1.0, offset: 0.0, signed: None, enabled: true,
        };
        assert_eq!(extract_value(&p, &f), None);
        assert_eq!(extract_value_broadcast(&p, &f), None);
        f.byte_index = None;
        f.bit_offset = Some(u32::MAX);
        f.bit_length = Some(8);
        assert_eq!(extract_value(&p, &f), None);
        assert_eq!(extract_value_broadcast(&p, &f), None);
        assert_eq!(extract_bits(&p, u32::MAX, 8), None);
    }

    #[test]
    fn assemble_iso_tp_short_reply_is_incomplete() {
        let lines = vec![b"014".to_vec(), b"0: 61 74 00 01 02 03".to_vec(), b"1: 04 05 06 07 08 09 0A".to_vec()];
        assert!(assemble_iso_tp(&lines).is_none());
    }

    #[test]
    fn assemble_iso_tp_decodes_lines_without_spaces() {
        let lines = vec![
            b"00D".to_vec(),
            b"0:7F2112AABBCC".to_vec(),
            b"1:2001020304050607".to_vec(),
        ];
        let p = assemble_iso_tp(&lines).expect("payload");
        assert_eq!(p, vec![0x7F, 0x21, 0x12, 0xAA, 0xBB, 0xCC, 0x20, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    }

    #[test]
    fn assemble_iso_tp_empty_returns_none() {
        let lines: Vec<Vec<u8>> = vec![];
        assert!(assemble_iso_tp(&lines).is_none());
    }

    #[test]
    fn route_broadcast_lines_groups_by_can_id() {
        // ATMA lines look like "673 11 22 33 44 55 66" — CAN-ID first, bytes after.
        let lines: Vec<String> = vec![
            "656 01 02 03 04 05 06 07".into(),
            "673 11 22 33 44 55 66".into(),
            "673 AA BB CC DD EE FF".into(),
            "ERROR".into(),
            "656 0A 0B 0C 0D 0E 0F 10".into(),
        ];
        let frames = route_broadcast_lines(&lines);
        assert_eq!(frames.get("673").map(|v| v.len()), Some(2));
        assert_eq!(frames.get("656").map(|v| v.len()), Some(2));
        assert_eq!(frames["673"][0], vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(frames["656"][1], vec![0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10]);
    }


    #[tokio::test(start_paused = true)]
    async fn send_cmd_reports_eof_as_a_lost_link() {
        use tokio::io::AsyncReadExt;
        let (a, mut b) = tokio::io::duplex(256);
        let mut session = Elm327Session::new(a);
        tokio::spawn(async move {
            let mut req = [0u8; 16];
            let _ = b.read(&mut req).await.unwrap();
            drop(b);
        });
        let e = session.send_cmd("ATZ").await.unwrap_err();
        let kind = e.downcast_ref::<std::io::Error>().map(std::io::Error::kind);
        assert_eq!(kind, Some(ErrorKind::UnexpectedEof), "{e:#}");
        assert!(matches!(classify(e), AdapterError::FatalConn(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn send_cmd_reply_cut_off_before_the_prompt_is_eof() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (a, mut b) = tokio::io::duplex(256);
        let mut session = Elm327Session::new(a);
        tokio::spawn(async move {
            let mut req = [0u8; 16];
            let _ = b.read(&mut req).await.unwrap();
            b.write_all(b"PARTIAL").await.unwrap();
            drop(b);
        });
        let e = session.send_cmd("2201").await.unwrap_err();
        let kind = e.downcast_ref::<std::io::Error>().map(std::io::Error::kind);
        assert_eq!(kind, Some(ErrorKind::UnexpectedEof), "{e:#}");
    }

    #[tokio::test]
    async fn send_cmd_no_data_reply_stays_transient() {
        use tokio::io::AsyncWriteExt;
        let (a, mut b) = tokio::io::duplex(256);
        let mut session = Elm327Session::new(a);
        b.write_all(b"NO DATA\r\r>").await.unwrap();
        let e = session.send_cmd("2201").await.unwrap_err();
        assert!(matches!(classify(e), AdapterError::Transient(_)));
    }

    #[tokio::test]
    async fn session_send_cmd_reads_until_prompt() {
        let (a, mut b) = tokio::io::duplex(256);
        let mut session = Elm327Session::new(a);

        let fixture = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut req = [0u8; 16];
            let n = b.read(&mut req).await.unwrap();
            assert_eq!(&req[..n], b"ATZ\r");
            b.write_all(b"ELM327 v1.5\r>").await.unwrap();
        });

        let resp = session.send_cmd("ATZ").await.unwrap();
        assert!(String::from_utf8_lossy(&resp).contains("ELM327"));
        fixture.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn session_keeps_bytes_received_after_a_prompt() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (a, mut b) = tokio::io::duplex(256);
        let mut session = Elm327Session::new(a);

        let fixture = tokio::spawn(async move {
            let mut req = [0u8; 16];
            let n = b.read(&mut req).await.unwrap();
            assert_eq!(&req[..n], b"ONE\r");
            // Both replies arrive in one write.
            b.write_all(b"A\r\r>B\r\r>").await.unwrap();
            let n = b.read(&mut req).await.unwrap();
            assert_eq!(&req[..n], b"TWO\r");
            b
        });

        let first = session.send_cmd("ONE").await.unwrap();
        assert!(String::from_utf8_lossy(&first).contains('A'));
        let second = session.send_cmd("TWO").await.unwrap();
        assert!(String::from_utf8_lossy(&second).contains('B'));
        fixture.await.unwrap();
    }
}
