//! The on-disk record format. This module is the only place that knows the bytes;
//! everything else treats records as opaque values.
//!
//! ```text
//! record  := [len: u32 LE][crc: u32 LE][payload]
//! payload := tag u8 | key_len u32 LE | key | value_len u32 LE | value
//! tag     := 0x00 Set | 0x01 Rm (value_len = 0, no value bytes)
//! crc     := CRC-32 of payload only
//! len     := payload length, so a record spans HEADER_LEN + len bytes
//! ```

use crate::error::{KvError, Result};

/// Largest payload accepted.
const MAX_RECORD: usize = 64 << 20; // 64 MiB

/// Size of [len][crc].
const HEADER_LEN: usize = 8;

const TAG_SET: u8 = 0x00;
const TAG_RM: u8 = 0x01;

enum Tag {
    Set,
    Rm,
}

impl Tag {
    fn from_byte(byte: u8) -> Result<Tag> {
        match byte {
            TAG_SET => Ok(Tag::Set),
            TAG_RM => Ok(Tag::Rm),
            other => Err(KvError::Corrupt(format!("unknown tag {other:#04x}"))),
        }
    }
}

/// One log entry (verb + operands)
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Set { key: String, value: String },
    Rm { key: String },
}

/// A parsed record header.
pub(crate) struct Header {
    /// Payload length, already checked against `MAX_RECORD`.
    pub(crate) payload_len: usize,
    /// CRC-32 the payload must match.
    pub(crate) crc: u32,
}

impl Header {
    pub(crate) const SIZE: usize = HEADER_LEN;

    /// Parses the header from the first `SIZE` bytes of `bytes`; any extra bytes
    /// are ignored, so a caller may pass a whole record.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_LEN {
            return Err(KvError::Corrupt(format!(
                "truncated header: {} bytes",
                bytes.len()
            )));
        }
        let payload_len = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes")) as usize;
        let crc = u32::from_le_bytes(bytes[4..HEADER_LEN].try_into().expect("4 bytes"));

        // `payload_len` came off disk and is untrusted.
        if payload_len > MAX_RECORD {
            return Err(KvError::Corrupt(format!(
                "record too large: {payload_len} bytes"
            )));
        }

        Ok(Header { payload_len, crc })
    }

    pub(crate) fn record_len(&self) -> usize {
        HEADER_LEN + self.payload_len
    }
}

pub(crate) fn encode(cmd: &Command, buf: &mut Vec<u8>) -> Result<()> {
    let payload_len = match cmd {
        Command::Set { key, value } => 1 + 4 + key.len() + 4 + value.len(),
        Command::Rm { key } => 1 + 4 + key.len() + 4,
    };
    if payload_len > MAX_RECORD {
        // Nothing has been touched, so the caller's buffer is unchanged.
        return Err(KvError::Corrupt(format!(
            "record too large: {payload_len} bytes"
        )));
    }

    buf.clear();
    buf.reserve(HEADER_LEN + payload_len);
    buf.extend_from_slice(&[0; HEADER_LEN]);

    match cmd {
        Command::Set { key, value } => {
            buf.push(TAG_SET);
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
            buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
            buf.extend_from_slice(value.as_bytes());
        }
        Command::Rm { key } => {
            buf.push(TAG_RM);
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
            // Omitting this would make the next field's bytes parse as a value length.
            buf.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    let crc = crc32fast::hash(&buf[HEADER_LEN..]);
    buf[..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    buf[4..HEADER_LEN].copy_from_slice(&crc.to_le_bytes());

    Ok(())
}

pub(crate) fn decode_payload(payload: &[u8], expected_crc: u32) -> Result<Command> {
    // Checksum before parsing
    if crc32fast::hash(payload) != expected_crc {
        return Err(KvError::Corrupt("checksum mismatch".to_owned()));
    }

    // Walk the payload with a cursor. Every field is read through `get`, which
    // fails instead of panicking when the payload is shorter than it should.
    let mut pos = 0;
    let tag = *payload
        .get(pos)
        .ok_or_else(|| KvError::Corrupt("empty payload".to_owned()))?;
    pos += 1;
    let tag = Tag::from_byte(tag)?;

    let key = take_string(payload, &mut pos)?;
    let cmd = match tag {
        Tag::Set => Command::Set {
            key,
            value: take_string(payload, &mut pos)?,
        },
        Tag::Rm => {
            // Rm still carries the field; a non-zero length means a broken writer.
            if take_u32(payload, &mut pos)? != 0 {
                return Err(KvError::Corrupt(
                    "Rm record with non-empty value".to_owned(),
                ));
            }
            Command::Rm { key }
        }
    };

    if pos != payload.len() {
        return Err(KvError::Corrupt(format!(
            "{} trailing bytes in record",
            payload.len() - pos
        )));
    }

    Ok(cmd)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<(Command, usize)> {
    let header = Header::decode(bytes)?;
    let end = header.record_len();

    if bytes.len() < end {
        return Err(KvError::Corrupt(format!(
            "truncated payload: need {} bytes, have {}",
            header.payload_len,
            bytes.len() - HEADER_LEN
        )));
    }

    let cmd = decode_payload(&bytes[HEADER_LEN..end], header.crc)?;
    Ok((cmd, end))
}

fn take_u32(payload: &[u8], pos: &mut usize) -> Result<u32> {
    let n = payload
        .get(*pos..)
        .and_then(|rest| rest.first_chunk::<4>())
        .ok_or_else(|| KvError::Corrupt("truncated length field".to_owned()))?;
    *pos += 4;

    Ok(u32::from_le_bytes(*n))
}

fn take_string(payload: &[u8], pos: &mut usize) -> Result<String> {
    let len = take_u32(payload, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| KvError::Corrupt("length overflow".to_owned()))?;
    let chunk = payload
        .get(*pos..end)
        .ok_or_else(|| KvError::Corrupt(format!("truncated field: need {len} bytes")))?;
    *pos = end;

    let s = std::str::from_utf8(chunk)
        .map_err(|_| KvError::Corrupt("invalid utf-8 in record".to_owned()))?;
    Ok(s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Command {
        Command::Set {
            key: "ab".into(),
            value: "c".into(),
        }
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut bytes = (payload.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn round_trip_set_and_rm() {
        let mut buf = Vec::new();
        let records = [
            Command::Set {
                key: "k".into(),
                value: "v".into(),
            },
            Command::Set {
                key: "e".into(),
                value: String::new(),
            },
            Command::Set {
                key: "ü".into(),
                value: "€".into(),
            },
            Command::Rm { key: "k".into() },
        ];
        for cmd in &records {
            encode(cmd, &mut buf).unwrap();
            let (got, consumed) = decode(&buf).unwrap();
            assert_eq!(&got, cmd);
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn header_then_payload_matches_whole_record() {
        let mut buf = Vec::new();
        encode(&sample(), &mut buf).unwrap();

        let header = Header::decode(&buf).unwrap();
        assert_eq!(header.record_len(), buf.len());
        let split = decode_payload(&buf[HEADER_LEN..header.record_len()], header.crc).unwrap();

        let (whole, consumed) = decode(&buf).unwrap();
        assert_eq!(split, whole);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn encode_rejects_oversized_record_and_leaves_buf_alone() {
        let mut buf = vec![1, 2, 3];
        let cmd = Command::Rm {
            key: "a".repeat(MAX_RECORD),
        };
        assert!(matches!(
            encode(&cmd, &mut buf),
            Err(KvError::Corrupt(m)) if m.starts_with("record too large")
        ));
        assert_eq!(buf, [1, 2, 3]);
    }

    #[test]
    fn truncated_header_is_corrupt() {
        let header = [0u8; HEADER_LEN];
        for n in [0, 1, HEADER_LEN - 1] {
            assert!(matches!(
                Header::decode(&header[..n]),
                Err(KvError::Corrupt(m)) if m.starts_with("truncated header")
            ));
        }
    }

    #[test]
    fn oversized_len_is_rejected_by_header() {
        let mut h = [0u8; HEADER_LEN];
        h[..4].copy_from_slice(&((MAX_RECORD + 1) as u32).to_le_bytes());
        assert!(matches!(
            Header::decode(&h),
            Err(KvError::Corrupt(m)) if m.starts_with("record too large")
        ));
    }

    #[test]
    fn max_record_len_is_accepted_by_header() {
        let mut h = [0u8; HEADER_LEN];
        h[..4].copy_from_slice(&(MAX_RECORD as u32).to_le_bytes());
        assert_eq!(Header::decode(&h).unwrap().payload_len, MAX_RECORD);
    }

    #[test]
    fn truncated_payload_is_corrupt() {
        let mut buf = Vec::new();
        encode(&sample(), &mut buf).unwrap();
        buf.pop();
        assert!(matches!(decode(&buf), Err(KvError::Corrupt(_))));
    }

    #[test]
    fn empty_payload_is_corrupt() {
        assert!(matches!(
            decode(&frame(&[])),
            Err(KvError::Corrupt(m)) if m.starts_with("empty payload")
        ));
    }

    #[test]
    fn unknown_tag_is_corrupt() {
        assert!(matches!(
            decode(&frame(&[0x02])),
            Err(KvError::Corrupt(m)) if m.starts_with("unknown tag")
        ));
    }

    #[test]
    fn field_longer_than_payload_is_corrupt() {
        let mut p = vec![TAG_SET];
        p.extend_from_slice(&5u32.to_le_bytes());
        p.push(b'k');
        assert!(matches!(
            decode(&frame(&p)),
            Err(KvError::Corrupt(m)) if m.starts_with("truncated field")
        ));
    }

    #[test]
    fn rm_with_non_empty_value_is_corrupt() {
        let mut p = vec![TAG_RM];
        p.extend_from_slice(&1u32.to_le_bytes());
        p.push(b'k');
        p.extend_from_slice(&1u32.to_le_bytes());
        p.push(b'x');
        assert!(matches!(
            decode(&frame(&p)),
            Err(KvError::Corrupt(m)) if m.contains("non-empty value")
        ));
    }

    #[test]
    fn trailing_garbage_with_valid_crc_is_corrupt() {
        let mut payload = vec![TAG_SET];
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(b'k');
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(b"v!");
        assert!(matches!(
            decode(&frame(&payload)),
            Err(KvError::Corrupt(m)) if m.contains("trailing bytes")
        ));
    }

    #[test]
    fn flipped_payload_byte_is_corrupt() {
        let mut buf = Vec::new();
        encode(&sample(), &mut buf).unwrap();
        *buf.last_mut().unwrap() ^= 0x01;
        assert!(matches!(
            decode(&buf),
            Err(KvError::Corrupt(m)) if m.starts_with("checksum mismatch")
        ));
    }

    #[test]
    fn flipped_crc_byte_is_corrupt() {
        let mut buf = Vec::new();
        encode(&sample(), &mut buf).unwrap();
        buf[4] ^= 0x01;
        assert!(matches!(
            decode(&buf),
            Err(KvError::Corrupt(m)) if m.starts_with("checksum mismatch")
        ));
    }
}
