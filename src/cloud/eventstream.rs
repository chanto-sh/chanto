//! AWS event-stream framing (`application/vnd.amazon.eventstream`), used by
//! Bedrock's streaming APIs:
//!
//! ```text
//! total_len u32 | headers_len u32 | prelude_crc u32 | headers | payload | message_crc u32
//! ```
//!
//! Both checksums are CRC-32 (IEEE); the prelude CRC covers the first 8 bytes and
//! the message CRC everything before it.

use anyhow::{Result, bail};

/// Frames larger than this are rejected as corrupt (Bedrock chunks are small).
const MAX_FRAME: usize = 16 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Int(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| match v {
                HeaderValue::String(s) => Some(s.as_str()),
                _ => None,
            })
    }
}

/// Incremental decoder: feed bytes as they arrive, take complete frames out.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes received that don't form a complete frame yet.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// The next complete frame, `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.buf.len() < 12 {
            return Ok(None);
        }
        let total = be32(&self.buf[0..4]) as usize;
        let headers_len = be32(&self.buf[4..8]) as usize;
        if crc32fast::hash(&self.buf[0..8]) != be32(&self.buf[8..12]) {
            bail!("event stream: prelude checksum mismatch");
        }
        if !(16..=MAX_FRAME).contains(&total) || headers_len > total - 16 {
            bail!("event stream: invalid frame length {total}");
        }
        if self.buf.len() < total {
            return Ok(None);
        }
        let frame: Vec<u8> = self.buf.drain(..total).collect();
        if crc32fast::hash(&frame[..total - 4]) != be32(&frame[total - 4..]) {
            bail!("event stream: message checksum mismatch");
        }
        let headers = parse_headers(&frame[12..12 + headers_len])?;
        Ok(Some(Frame {
            headers,
            payload: frame[12 + headers_len..total - 4].to_vec(),
        }))
    }
}

fn parse_headers(mut b: &[u8]) -> Result<Vec<(String, HeaderValue)>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
        if b.len() < n {
            bail!("event stream: truncated header");
        }
        let (head, rest) = b.split_at(n);
        *b = rest;
        Ok(head)
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let n = take(&mut b, 1)?[0] as usize;
        let name = String::from_utf8_lossy(take(&mut b, n)?).into_owned();
        let ty = take(&mut b, 1)?[0];
        let int = |b: &mut &[u8], n: usize| -> Result<i64> {
            let raw = take(b, n)?;
            let mut v: i64 = if raw[0] & 0x80 != 0 { -1 } else { 0 };
            for x in raw {
                v = (v << 8) | *x as i64;
            }
            Ok(v)
        };
        let value = match ty {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Int(int(&mut b, 1)?),
            3 => HeaderValue::Int(int(&mut b, 2)?),
            4 => HeaderValue::Int(int(&mut b, 4)?),
            5 => HeaderValue::Int(int(&mut b, 8)?),
            6 | 7 => {
                let len = u16::from_be_bytes([take(&mut b, 1)?[0], take(&mut b, 1)?[0]]) as usize;
                let raw = take(&mut b, len)?;
                if ty == 6 {
                    HeaderValue::Bytes(raw.to_vec())
                } else {
                    HeaderValue::String(String::from_utf8_lossy(raw).into_owned())
                }
            }
            8 => HeaderValue::Timestamp(int(&mut b, 8)?),
            9 => HeaderValue::Uuid(take(&mut b, 16)?.try_into().unwrap()),
            t => bail!("event stream: unknown header type {t}"),
        };
        out.push((name, value));
    }
    Ok(out)
}

/// Encode a frame with string headers (tests and mock servers).
#[cfg(test)]
pub fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (k, v) in headers {
        h.push(k.len() as u8);
        h.extend_from_slice(k.as_bytes());
        h.push(7);
        h.extend_from_slice(&(v.len() as u16).to_be_bytes());
        h.extend_from_slice(v.as_bytes());
    }
    let total = 12 + h.len() + payload.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out
}
