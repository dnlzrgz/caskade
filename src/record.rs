//! The on-disk record format. This module is the only place that knows the bytes;
//! everything else treats records as opaque values.
//!
//! ```text
//! record  := [len: u32 LE][crc: u32 LE][hcrc: u32 LE][payload]
//! payload := tag u8 | key_len u32 LE | key | value_len u32 LE | value
//! tag     := 0x00 Set | 0x01 Rm (value_len = 0, no value bytes)
//! crc     := CRC-32 of payload only
//! hcrc    := CRC-32 of [len][crc]. Without it a damaged length cannot be told
//!            apart from a payload cut short by a crash, and replay would throw
//!            away the good records behind it.
//! len     := payload length, so a record spans HEADER_LEN + len bytes
//! ```
//!
//! The layout is not versioned: changing it makes existing logs unreadable.

use crate::error::{KvError, Result};

/// Largest payload accepted.
const MAX_RECORD: usize = 64 << 20; // 64 MiB
/// Size of [len][crc][hcrc].
const HEADER_LEN: usize = 12;

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
pub(crate) enum Command<'a> {
    Set { key: &'a str, value: &'a str },
    Rm { key: &'a str },
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

    /// Parses the header from the first `SIZE` bytes of `bytes`. Extra bytes are
    /// ignored, so a caller may pass a whole record.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_LEN {
            return Err(KvError::Corrupt(format!(
                "truncated header: {} bytes",
                bytes.len()
            )));
        }
        let payload_len = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes")) as usize;
        let crc = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
        let hcrc = u32::from_le_bytes(bytes[8..HEADER_LEN].try_into().expect("4 bytes"));

        // A length read from disk is only trustworthy if its own checksum matches.
        if crc32fast::hash(&bytes[..8]) != hcrc {
            return Err(KvError::Corrupt("header checksum mismatch".to_owned()));
        }

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

pub(crate) fn encode(cmd: &Command<'_>, buf: &mut Vec<u8>) -> Result<()> {
    let payload_len = match cmd {
        Command::Set { key, value } => 1 + 4 + key.len() + 4 + value.len(),
        Command::Rm { key } => 1 + 4 + key.len() + 4,
    };
    if payload_len > MAX_RECORD {
        // Nothing has been touched, so the caller's buffer is unchanged.
        return Err(KvError::RecordTooLarge(payload_len));
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
            buf.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    let crc = crc32fast::hash(&buf[HEADER_LEN..]);
    buf[..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
    let hcrc = crc32fast::hash(&buf[..8]);
    buf[8..HEADER_LEN].copy_from_slice(&hcrc.to_le_bytes());

    Ok(())
}

pub(crate) fn decode_payload(payload: &[u8], expected_crc: u32) -> Result<Command<'_>> {
    // Checksum before parsing
    if crc32fast::hash(payload) != expected_crc {
        return Err(KvError::ChecksumMismatch);
    }

    // Walk the payload with a cursor. Every field is read through `get`, which
    // fails when the payload is shorter than it should.
    let mut pos = 0;
    let tag = *payload
        .get(pos)
        .ok_or_else(|| KvError::Corrupt("empty payload".to_owned()))?;
    pos += 1;
    let tag = Tag::from_byte(tag)?;

    let key = take_str(payload, &mut pos)?;
    let cmd = match tag {
        Tag::Set => Command::Set {
            key,
            value: take_str(payload, &mut pos)?,
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

fn take_u32(payload: &[u8], pos: &mut usize) -> Result<u32> {
    let n = payload
        .get(*pos..)
        .and_then(|rest| rest.first_chunk::<4>())
        .ok_or_else(|| KvError::Corrupt("truncated length field".to_owned()))?;
    *pos += 4;

    Ok(u32::from_le_bytes(*n))
}

fn take_str<'a>(payload: &'a [u8], pos: &mut usize) -> Result<&'a str> {
    let len = take_u32(payload, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| KvError::Corrupt("length overflow".to_owned()))?;
    let chunk = payload
        .get(*pos..end)
        .ok_or_else(|| KvError::Corrupt(format!("truncated field: need {len} bytes")))?;
    *pos = end;

    std::str::from_utf8(chunk).map_err(|_| KvError::Corrupt("invalid utf-8 in record".to_owned()))
}

#[cfg(test)]
pub(crate) fn decode(bytes: &[u8]) -> Result<(Command<'_>, usize)> {
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

#[cfg(test)]
/// Frames `payload` as a record, as a well-behaved writer would.
pub(crate) fn frame(payload: &[u8]) -> Vec<u8> {
    let mut bytes = (payload.len() as u32).to_le_bytes().to_vec();
    bytes.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    let hcrc = crc32fast::hash(&bytes[..8]);
    bytes[8..HEADER_LEN].copy_from_slice(&hcrc.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Command<'static> {
        Command::Set {
            key: "ab",
            value: "c",
        }
    }

    /// A header for `payload_len`, sealed with a valid checksum.
    fn header_bytes(payload_len: usize) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
        let hcrc = crc32fast::hash(&h[..8]);
        h[8..].copy_from_slice(&hcrc.to_le_bytes());
        h
    }

    #[test]
    fn round_trip_set_and_rm() {
        let mut buf = Vec::new();
        let records = [
            Command::Set {
                key: "k",
                value: "v",
            },
            Command::Set {
                key: "e",
                value: "",
            },
            Command::Set {
                key: "ü",
                value: "€",
            },
            Command::Rm { key: "k" },
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
        let key = "a".repeat(MAX_RECORD);
        let cmd = Command::Rm { key: &key };
        assert!(matches!(
            encode(&cmd, &mut buf),
            Err(KvError::RecordTooLarge(_))
        ));
        assert_eq!(buf, [1, 2, 3]);
    }

    #[test]
    fn truncated_header_is_corrupt() {
        let header = header_bytes(16);
        for n in [0, 1, HEADER_LEN - 1] {
            assert!(matches!(
                Header::decode(&header[..n]),
                Err(KvError::Corrupt(m)) if m.starts_with("truncated header")
            ));
        }
    }

    #[test]
    fn oversized_len_is_rejected_by_header() {
        let h = header_bytes(MAX_RECORD + 1);
        assert!(matches!(
            Header::decode(&h),
            Err(KvError::Corrupt(m)) if m.starts_with("record too large")
        ));
    }

    #[test]
    fn max_record_len_is_accepted_by_header() {
        let h = header_bytes(MAX_RECORD);
        assert_eq!(Header::decode(&h).unwrap().payload_len, MAX_RECORD);
    }

    #[test]
    fn damaged_header_is_corrupt() {
        let mut h = header_bytes(16);
        h[0] ^= 0x01; // flip a byte of the length, leave the checksum alone
        assert!(matches!(
            Header::decode(&h),
            Err(KvError::Corrupt(m)) if m.starts_with("header checksum mismatch")
        ));
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
        let last = buf.len() - 1;
        buf[last] ^= 0x01;
        assert!(matches!(decode(&buf), Err(KvError::ChecksumMismatch)));
    }

    #[test]
    fn flipped_crc_byte_is_corrupt() {
        let mut buf = Vec::new();
        encode(&sample(), &mut buf).unwrap();
        buf[4] ^= 0x01;
        assert!(matches!(
            decode(&buf),
            Err(KvError::Corrupt(m)) if m.starts_with("header checksum mismatch")
        ));
    }

    #[test]
    fn invalid_utf8_is_corrupt() {
        let mut p = vec![TAG_RM];
        p.extend_from_slice(&1u32.to_le_bytes());
        p.push(0xFF);
        p.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode(&frame(&p)),
            Err(KvError::Corrupt(m)) if m.starts_with("invalid utf-8")
        ));
    }
}
