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

/// Extract an f32 from a UDS payload according to a FieldSpec. Byte-aligned
/// extraction only (bit extraction arrives in a later task).
pub fn extract_value(payload: &[u8], field: &FieldSpec) -> Option<f32> {
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
