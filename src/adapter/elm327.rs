// ELM327 session helper, generic over an async byte stream
// (T: AsyncRead + AsyncWrite + Unpin) so it can drive both Bluetooth RFCOMM
// and USB serial transports.

use crate::profile::{BroadcastSource, FieldSpec, UdsPidSource};
use anyhow::{anyhow, Context, Result};
use log::{debug, trace};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::io::ErrorKind;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

/// Default ELM327 initialisation sequence. A profile may override it via
/// elm327.init.
pub const DEFAULT_INIT: &[&str] = &[
    "ATZ", "ATE0", "ATAL", "ATST96", "ATCP18", "ATFCSD300000", "ATSP6",
];

const EOM_PROMPT: u8 = b'>';
const SEND_CMD_TIMEOUT: Duration = Duration::from_millis(5000);

pub struct Elm327Session<T> {
    pub stream: T,
}

impl<T> Elm327Session<T>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    pub fn new(stream: T) -> Self { Self { stream } }

    /// Send a command, read until the '>' prompt, return the raw bytes.
    pub async fn send_cmd(&mut self, cmd: &str) -> Result<Vec<u8>> {
        use std::io::{Error as IoError, ErrorKind};
        let mut wire = Vec::with_capacity(cmd.len() + 1);
        wire.extend_from_slice(cmd.as_bytes());
        wire.push(b'\r');
        debug!("write: {}", String::from_utf8_lossy(&wire));

        if let Err(e) = self.stream.write_all(&wire).await {
            return Err(anyhow::Error::new(IoError::new(e.kind(), format!("write '{cmd}': {e}"))));
        }

        let mut buf = Vec::new();
        let mut reader = BufReader::new(&mut self.stream);

        match timeout(SEND_CMD_TIMEOUT, reader.read_until(EOM_PROMPT, &mut buf)).await {
            Ok(Ok(0)) => Err(anyhow::Error::new(IoError::new(ErrorKind::Other, format!("0 bytes for '{cmd}'")))),
            Ok(Ok(_)) => {
                let ascii = String::from_utf8_lossy(&buf);
                trace!("Response ASCII: {ascii}");
                if ascii.contains("NO DATA") {
                    return Err(anyhow::Error::new(IoError::new(ErrorKind::Other, "no data")));
                }
                if ascii.contains("7F 22 12") {
                    return Err(anyhow::Error::new(IoError::new(ErrorKind::Other, "Service Not Supported")));
                }
                Ok(buf)
            }
            Ok(Err(e)) => Err(anyhow::Error::new(IoError::new(e.kind(), format!("read '{cmd}': {e}")))),
            Err(_) => Err(anyhow::Error::new(IoError::new(ErrorKind::TimedOut, format!("timeout '{cmd}'")))),
        }
    }

    /// Run a sequence of ELM327 init commands. Each failure is fatal.
    pub async fn run_init(&mut self, init: &[&str]) -> Result<()> {
        for cmd in init {
            self.send_cmd(cmd).await
                .with_context(|| format!("init step '{cmd}' failed"))?;
        }
        Ok(())
    }

    /// Send a uds_pid source and return the single-frame response payload.
    pub async fn poll_uds_pid(&mut self, uds: &UdsPidSource) -> Result<Vec<u8>> {
        self.send_cmd(&format!("ATSH{}", uds.ecu_tx)).await?;
        self.send_cmd(&format!("ATCRA{}", uds.ecu_rx)).await?;
        self.send_cmd(&format!("ATFCSH{}", uds.ecu_tx)).await?;
        self.send_cmd("ATFCSD300000").await?;
        self.send_cmd("ATFCSM1").await?;
        if let Some(pre) = &uds.pre_request {
            // Pre-request errors are intentionally ignored — some ECUs don't ACK.
            let _ = self.send_cmd(pre).await;
        }
        let raw = self.send_cmd(&uds.pid).await?;
        Ok(get_payload(&String::from_utf8_lossy(&raw)))
    }

    /// Poll a uds_pid source with ISO-TP flow-control reassembly, for responses
    /// that span multiple frames. Used when the source sets multiframe = true.
    pub async fn poll_uds_pid_multiframe(&mut self, uds: &UdsPidSource) -> Result<Vec<u8>> {
        let sh   = u32::from_str_radix(&uds.ecu_tx, 16).context("ecu_tx not hex")?;
        let cra  = u32::from_str_radix(&uds.ecu_rx, 16).context("ecu_rx not hex")?;
        let fcsh = sh;

        self.send_cmd(&format!("ATSH{:x}", sh)).await?;
        self.send_cmd(&format!("ATCRA{:x}", cra)).await?;
        self.send_cmd(&format!("ATFCSH{:x}", fcsh)).await?;
        self.send_cmd("ATFCSD300000").await?;
        self.send_cmd("ATFCSM1").await?;
        self.send_cmd("ATH0").await?;

        // Send the PID directly (then read lines rather than wait for a prompt).
        let cmd = format!("{}\r", uds.pid);
        self.stream.write_all(cmd.as_bytes()).await
            .context("multiframe PID write")?;

        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut fc_sent = false;

        for _ in 0..25 {
            let line = match read_line_raw(&mut self.stream, 500).await {
                Ok(l) => l,
                Err(e) if e.kind() == ErrorKind::TimedOut => break,
                Err(e) => {
                    let _ = self.send_cmd("ATH0").await;
                    return Err(anyhow!("multiframe read: {e}"));
                }
            };
            let trimmed = String::from_utf8_lossy(&line).trim().to_string();
            if trimmed.is_empty() || trimmed == ">" { break; }
            if trimmed.contains("NO DATA") || trimmed.contains("7F") {
                let _ = self.send_cmd("ATH0").await;
                return Err(anyhow!("UDS error response: {trimmed}"));
            }
            lines.push(line);

            // After the first ECU frame, send a flow-control frame so the ECU
            // streams the remaining consecutive frames.
            if !fc_sent && lines.len() == 1 {
                self.stream.write_all(b"300200\r").await
                    .context("FC frame write")?;
                fc_sent = true;
            }
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
            let _ = self.send_cmd(cmd).await;
        }
        // ATMA streams continuously; do not wait for a '>' prompt.
        self.stream.write_all(format!("{}\r", spec.command).as_bytes()).await
            .context("broadcast command write")?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(spec.deadline_ms);
        let mut lines: Vec<String> = Vec::new();
        let mut consecutive_timeouts: u64 = 0;
        let idle_attempts = ((spec.idle_timeout_ms + 199) / 200).max(1);

        // CAN-IDs whose frame carries a field listed in stop_when. Once a line
        // has arrived for each of them, the wanted data is in hand and the scan
        // can end before the deadline.
        let needed_ids: HashSet<String> = spec.frames.iter()
            .filter(|f| f.fields.iter().any(|fld| spec.stop_when.iter().any(|s| s == &fld.name)))
            .map(|f| f.can_id.clone())
            .collect();
        let mut seen_ids: HashSet<String> = HashSet::new();

        while tokio::time::Instant::now() < deadline {
            match read_line_raw(&mut self.stream, 200).await {
                Ok(b) => {
                    consecutive_timeouts = 0;
                    let ascii = String::from_utf8_lossy(&b).into_owned();
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
                Err(e) => return Err(anyhow!("broadcast read: {e}")),
            }
        }

        // Stop ATMA cleanly: send a CR, then drain until the '>' prompt.
        let _ = self.stream.write_all(b"\r").await;
        let mut byte = [0u8; 1];
        for _ in 0..200 {
            match timeout(Duration::from_millis(50), self.stream.read(&mut byte)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) if byte[0] == b'>' => break,
                Ok(Err(_)) | Err(_) => break,
                _ => {}
            }
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
                    for fld in &frame.fields {
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

/// Strip ELM327 framing from a raw response string and return the UDS payload
/// bytes (header + data).
pub fn get_payload(response: &str) -> Vec<u8> {
    let frames: Vec<&str> = response.split(|c| c == '\r' || c == '\n')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| !s.contains("SEARCHING"))
        .collect();

    let mut payload = Vec::new();
    let mut is_first = true;

    for frame in frames {
        let cleaned = frame.replace(' ', "");
        if is_first && cleaned.len() <= 3 {
            is_first = false;
            continue;
        }
        is_first = false;

        let data_str = if cleaned.contains(':') {
            cleaned.split(':').nth(1).unwrap_or("").to_string()
        } else {
            cleaned
        };

        let mut bytes = Vec::new();
        let mut chars = data_str.chars();
        while let (Some(c1), Some(c2)) = (chars.next(), chars.next()) {
            if let Ok(b) = u8::from_str_radix(&format!("{c1}{c2}"), 16) {
                bytes.push(b);
            }
        }
        if !bytes.is_empty() { payload.extend(bytes); }
    }
    payload
}

/// Extract a packed value using Motorola/big-endian bit numbering — bit 0 is
/// the MSB of byte 0. bit_length is 1..=16. Reads contiguously across bytes.
fn extract_bits(payload: &[u8], bit_offset: u32, bit_length: u32) -> Option<u32> {
    if bit_length == 0 || bit_length > 16 { return None; }
    let last_bit = bit_offset + bit_length;
    let needed_bytes = ((last_bit + 7) / 8) as usize;
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
    // Bit branch first — explicit opt-in via bit_offset+bit_length.
    if let (Some(bit_off), Some(bit_len)) = (field.bit_offset, field.bit_length) {
        return extract_bits(payload, bit_off, bit_len)
            .map(|v| (v as f32) * field.multiplier + field.offset);
    }

    // Byte branch (unchanged):
    let byte_index = field.byte_index?;
    let length     = field.length?;
    let idx = if byte_index < 0 {
        let positive = payload.len() as i32 + byte_index;
        if positive < 0 { return None; }
        positive as usize
    } else {
        // +3 skips the UDS positive-response header (e.g. 62 01 05).
        (byte_index + 3) as usize
    };
    if idx + length > payload.len() { return None; }

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

/// Read a single line (terminated by \r or \n) from the stream, with a
/// per-read timeout. Returns the line bytes excluding the terminator.
pub async fn read_line_raw<T>(stream: &mut T, millis: u64) -> std::io::Result<Vec<u8>>
where T: tokio::io::AsyncRead + Unpin,
{
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match timeout(Duration::from_millis(millis), stream.read(&mut byte)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                if byte[0] == b'\r' || byte[0] == b'\n' {
                    if !out.is_empty() { break; }
                } else {
                    out.push(byte[0]);
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                if out.is_empty() {
                    return Err(std::io::Error::new(ErrorKind::TimedOut, "line read timeout"));
                }
                break;
            }
        }
    }
    Ok(out)
}

/// Reassemble an ELM327 multi-line UDS response. With headers off and
/// automatic flow control, the adapter emits an optional total-length line
/// followed by data lines; non-hex tokens (line counters, the ">" prompt) are
/// filtered out by the hex parse. The first data line is taken wholesale; a
/// genuine 0x2N consecutive frame has its sequence byte stripped. The UDS
/// response header is retained, matching the single-frame path.
pub fn assemble_iso_tp(lines: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut payload: Vec<u8> = Vec::new();
    let mut expected_len: Option<usize> = None;
    let mut first_line = true;
    let mut got_data = false;

    for line in lines {
        let ascii = String::from_utf8_lossy(line);
        let bytes: Vec<u8> = ascii
            .split_whitespace()
            .filter_map(|t| u8::from_str_radix(t, 16).ok())
            .collect();
        if bytes.is_empty() { continue; }

        if first_line {
            if bytes.len() == 1 {
                // bare total-length header (e.g. ELM prints "012")
                expected_len = Some(bytes[0] as usize);
            } else {
                // first data line — taken wholesale (no PCI pair to strip)
                expected_len = expected_len.or(Some(18));
                payload.extend_from_slice(&bytes);
                first_line = false;
                got_data = true;
            }
        } else {
            // consecutive frame: strip the 0x2N sequence byte if present
            if bytes.len() > 1 && (bytes[0] & 0xF0) == 0x20 {
                payload.extend_from_slice(&bytes[1..]);
            } else {
                payload.extend_from_slice(&bytes);
            }
        }
        if let Some(exp) = expected_len {
            if got_data && payload.len() >= exp { break; }
        }
    }
    if !got_data { return None; }
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
    if idx + length > payload.len() { return None; }
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
            multiplier: 0.5, offset: 0.0, signed: None,
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
            multiplier: 1.0, offset: 0.0, signed: Some(true),
        };
        assert_eq!(extract_value(&payload, &f), Some(-10.0));
    }

    #[test]
    fn extract_negative_byte_index_from_end() {
        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22];
        let f = FieldSpec {
            name: "x".into(), byte_index: Some(-3), length: Some(3),
            bit_offset: None, bit_length: None,
            multiplier: 1.0, offset: 0.0, signed: None,
        };
        assert_eq!(extract_value(&payload, &f), Some(4386.0));
    }

    #[test]
    fn extract_out_of_bounds_returns_none() {
        let payload = vec![0u8; 4];
        let f = FieldSpec {
            name: "x".into(), byte_index: Some(10), length: Some(1),
            bit_offset: None, bit_length: None,
            multiplier: 1.0, offset: 0.0, signed: None,
        };
        assert_eq!(extract_value(&payload, &f), None);
    }

    #[test]
    fn get_payload_handles_single_frame_with_colon() {
        let raw = "0:6201050011 22\r\n>";
        let p = get_payload(raw);
        assert_eq!(p, vec![0x62, 0x01, 0x05, 0x00, 0x11, 0x22]);
    }

    #[test]
    fn get_payload_strips_length_header_first_frame() {
        let raw = "014\r0:62010511 22 33\r\n>";
        let p = get_payload(raw);
        assert_eq!(p, vec![0x62, 0x01, 0x05, 0x11, 0x22, 0x33]);
    }

    // Motorola bit numbering: bit 0 = MSB of byte 0.
    //   byte 0 = 0xA5 = 1010_0101 — bits 0..7
    //   byte 1 = 0x3C = 0011_1100 — bits 8..15

    #[test]
    fn extract_single_bit_msb() {
        let p = vec![0xA5, 0x3C, 0xFF, 0x00];
        let f = FieldSpec {
            name: "x".into(), byte_index: None, length: None,
            bit_offset: Some(0), bit_length: Some(1),
            multiplier: 1.0, offset: 0.0, signed: None,
        };
        assert_eq!(extract_value(&p, &f), Some(1.0));
    }

    #[test]
    fn extract_three_bit_value_spanning_byte_boundary() {
        // bits 6..8 of 0xA5 0x3C: byte0 last two bits = 01, byte1 first bit = 0 -> 010 = 2
        let p = vec![0xA5, 0x3C];
        let f = FieldSpec {
            name: "x".into(), byte_index: None, length: None,
            bit_offset: Some(6), bit_length: Some(3),
            multiplier: 1.0, offset: 0.0, signed: None,
        };
        assert_eq!(extract_value(&p, &f), Some(2.0));
    }

    #[test]
    fn extract_full_byte_via_bit_field() {
        // bits 8..15 of 0xA5 0x3C = 0x3C = 60
        let p = vec![0xA5, 0x3C];
        let f = FieldSpec {
            name: "x".into(), byte_index: None, length: None,
            bit_offset: Some(8), bit_length: Some(8),
            multiplier: 1.0, offset: 0.0, signed: None,
        };
        assert_eq!(extract_value(&p, &f), Some(60.0));
    }

    #[test]
    fn extract_bit_out_of_range_returns_none() {
        let p = vec![0xFF, 0xFF];
        let f = FieldSpec {
            name: "x".into(), byte_index: None, length: None,
            bit_offset: Some(20), bit_length: Some(4),
            multiplier: 1.0, offset: 0.0, signed: None,
        };
        assert_eq!(extract_value(&p, &f), None);
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
    fn assemble_iso_tp_strips_consecutive_pci_nibble() {
        let lines = vec![
            b"10 0A 61 74 11 22 33 44".to_vec(), // first line wholesale (incl 10 0A)
            b"21 55 66 77 88 99 AA".to_vec(),     // 0x2N consecutive -> strip seq byte
        ];
        let p = assemble_iso_tp(&lines).expect("payload");
        assert_eq!(&p[0..4], &[0x10, 0x0A, 0x61, 0x74]);
        assert_eq!(&p[8..14], &[0x55, 0x66, 0x77, 0x88, 0x99, 0xAA]);
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
}
