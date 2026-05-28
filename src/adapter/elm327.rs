// src/adapter/elm327.rs
//
// ELM327 session helper, generic over an async byte stream (T: AsyncRead +
// AsyncWrite + Unpin). Same protocol as the previous inline main.rs code —
// moved here so both the Bluetooth and (later) USB adapters share it.

use crate::profile::{FieldSpec, UdsPidSource};
use anyhow::{Context, Result};
use log::{debug, trace};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

/// Default init sequence (preserved from v0.2.0). A profile may override via
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

    /// Send a uds_pid source, return the assembled payload (single-frame for
    /// now; ISO-TP multiframe arrives in a later task).
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
}

/// Strip ELM327 framing from a raw response string and return the UDS payload
/// bytes (header + data). Mirrors the previous main.rs `get_payload`.
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
