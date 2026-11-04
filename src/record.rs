//! The on-disk record format. This module is the only place that knows the bytes;
//! everything else treats records as opaque values.
//!
//! ```text
//! file    := [magic "caskade\n"][version: u32 LE] record*
//! record  := [len: u32 LE][crc: u32 LE][hcrc: u32 LE][payload]
//! payload := tag u8 | key_len: varint | key | value   (Set: value is the rest)
//! payload := tag u8 | key_len: varint | key           (Rm)
//! tag     := 0x00 Set | 0x01 Rm
//! varint  := unsigned LEB128, at most 4 bytes
//! key, value := opaque bytes (no encoding is assumed)
//! crc     := CRC-32 of payload only
//! hcrc    := CRC-32 of [len][crc]. Without it a damaged length cannot be told
//!            apart from a payload cut short by a crash, and replay would throw
//!            away the good records behind it.
//! len     := payload length, so a record spans HEADER_LEN + len bytes
//! ```
//!

use crate::error::{KvError, Result};

/// Largest payload accepted.
pub(crate) const MAX_RECORD: usize = 64 << 20; // 64 MiB
/// Size of [len][crc][hcrc].
const HEADER_LEN: usize = 12;
/// File marker, followed by a `u32` version.
const MAGIC: [u8; 8] = *b"caskade\n";
/// Size of [magic][version], the prefix every log starts with.
pub(crate) const FILE_HEADER_LEN: usize = MAGIC.len() + 4;
/// Bumped whenever the bytes above change meaning.
pub(crate) const VERSION: u32 = 1;

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
    Set { key: &'a [u8], value: &'a [u8] },
    Rm { key: &'a [u8] },
}

/// A parsed record header.
pub(crate) struct Header {
    /// Payload length, already checked against `MAX_RECORD`.
    pub(crate) payload_len: usize,
    /// CRC-32 the payload must match.
    pub(crate) crc: u32,
}

impl Command<'_> {
    /// Length of the key, whatever the verb is.
    pub(crate) fn key_len(&self) -> usize {
        match self {
            Command::Set { key, .. } | Command::Rm { key } => key.len(),
        }
    }
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

/// The prefix an empty log is started with.
pub(crate) fn file_header() -> [u8; FILE_HEADER_LEN] {
    let mut head = [0u8; FILE_HEADER_LEN];
    head[..MAGIC.len()].copy_from_slice(&MAGIC);
    head[MAGIC.len()..].copy_from_slice(&VERSION.to_le_bytes());

    head
}

/// Bytes one record occupies in the log, header included. An `Rm` has no value
/// field, so `record_len(key_len, 0)` is its size too.
pub(crate) fn record_len(key_len: usize, value_len: usize) -> usize {
    HEADER_LEN + 1 + varint_len(key_len) + key_len + value_len
}

/// Offset of the value bytes of a record that starts at `record_pos`: where an
/// `Rm` of the same key would have ended.
pub(crate) fn value_offset(record_pos: u64, key_len: usize) -> u64 {
    record_pos + record_len(key_len, 0) as u64
}

/// Accepts only a log this build knows how to read.
pub(crate) fn check_file_header(head: &[u8]) -> Result<()> {
    if head.len() < FILE_HEADER_LEN {
        return Err(KvError::UnsupportedFormat(format!(
            "truncated file header: {} bytes",
            head.len()
        )));
    }
    if head[..MAGIC.len()] != MAGIC {
        return Err(KvError::UnsupportedFormat("not a caskade log".to_owned()));
    }
    let version = head
        .get(MAGIC.len()..FILE_HEADER_LEN)
        .and_then(|rest| rest.first_chunk::<4>())
        .ok_or_else(|| KvError::UnsupportedFormat("truncated file header".to_owned()))?;
    let version = u32::from_le_bytes(*version);
    if version != VERSION {
        return Err(KvError::UnsupportedFormat(format!(
            "log format version {version}, expected {VERSION}"
        )));
    }

    Ok(())
}

pub(crate) fn encode(cmd: &Command<'_>, buf: &mut Vec<u8>) -> Result<()> {
    let payload_len = match cmd {
        Command::Set { key, value } => 1 + varint_len(key.len()) + key.len() + value.len(),
        Command::Rm { key } => 1 + varint_len(key.len()) + key.len(),
    };
    if payload_len > MAX_RECORD {
        // Nothing has been touched, so the caller's buffer is unchanged.
        return Err(KvError::RecordTooLarge(payload_len));
    }

    buf.clear();
    buf.reserve(HEADER_LEN + payload_len);
    buf.extend_from_slice(&[0; HEADER_LEN]);

    let (tag, key, value) = match cmd {
        Command::Set { key, value } => (TAG_SET, *key, Some(*value)),
        Command::Rm { key } => (TAG_RM, *key, None),
    };
    buf.push(tag);
    // The check above bounds every field, so this cannot truncate.
    put_varint(key.len() as u32, buf);
    buf.extend_from_slice(key);
    if let Some(value) = value {
        buf.extend_from_slice(value);
    }
    debug_assert_eq!(buf.len(), HEADER_LEN + payload_len);

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

    let key = take_bytes(payload, &mut pos)?;
    let cmd = match tag {
        Tag::Set => {
            // No value length: the value is whatever is left.
            let value = &payload[pos..];
            pos = payload.len();
            Command::Set { key, value }
        }
        Tag::Rm => Command::Rm { key },
    };

    // Only `Rm` can leave a tail, and a writer never does.
    if pos != payload.len() {
        return Err(KvError::Corrupt(format!(
            "{} trailing bytes in record",
            payload.len() - pos
        )));
    }

    Ok(cmd)
}

/// Unsigned LEB128: one byte below 128, one more per 7 bits after that.
fn put_varint(mut n: u32, buf: &mut Vec<u8>) {
    while n >= 0x80 {
        buf.push((n as u8) | 0x80);
        n >>= 7;
    }
    buf.push(n as u8);
}

const fn varint_len(mut n: usize) -> usize {
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }

    len
}

/// Reads a varint of at most 4 bytes, which bounds it below `u32::MAX`.
fn take_varint(payload: &[u8], pos: &mut usize) -> Result<u32> {
    let mut n = 0u32;
    for shift in [0, 7, 14, 21] {
        let byte = *payload
            .get(*pos)
            .ok_or_else(|| KvError::Corrupt("truncated length field".to_owned()))?;
        *pos += 1;
        n |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(n);
        }
    }

    Err(KvError::Corrupt("length field too long".to_owned()))
}

fn take_bytes<'a>(payload: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let len = take_varint(payload, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| KvError::Corrupt("length overflow".to_owned()))?;
    let chunk = payload
        .get(*pos..end)
        .ok_or_else(|| KvError::Corrupt(format!("truncated field: need {len} bytes")))?;
    *pos = end;

    Ok(chunk)
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
    use proptest::prelude::*;

    use super::*;

    fn sample() -> Command<'static> {
        Command::Set {
            key: b"ab",
            value: b"c",
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
                key: b"k",
                value: b"v",
            },
            Command::Set {
                key: b"e",
                value: b"",
            },
            Command::Set {
                key: "ü".as_bytes(),
                value: "€".as_bytes(),
            },
            Command::Set {
                key: b"\xff\xfe",
                value: b"\x00\x80\xff",
            },
            Command::Rm { key: b"k" },
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
        let key = vec![b'a'; MAX_RECORD];
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
    fn file_header_is_accepted() {
        assert!(check_file_header(&file_header()).is_ok());
    }

    #[test]
    fn file_header_with_other_magic_is_unsupported() {
        let mut head = file_header();
        head[0] = b'x';
        assert!(matches!(
            check_file_header(&head),
            Err(KvError::UnsupportedFormat(m)) if m.contains("not a caskade log")
        ));
    }

    #[test]
    fn file_header_with_other_version_is_unsupported() {
        let mut head = file_header();
        head[MAGIC.len()..].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert!(matches!(
            check_file_header(&head),
            Err(KvError::UnsupportedFormat(m)) if m.contains("version 2, expected 1")
        ));
    }

    #[test]
    fn short_file_header_is_unsupported() {
        assert!(matches!(
            check_file_header(&file_header()[..FILE_HEADER_LEN - 1]),
            Err(KvError::UnsupportedFormat(m)) if m.starts_with("truncated file header")
        ));
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
        p.push(5); // key_len varint
        p.push(b'k');
        assert!(matches!(
            decode(&frame(&p)),
            Err(KvError::Corrupt(m)) if m.starts_with("truncated field")
        ));
    }

    #[test]
    fn overlong_length_field_is_corrupt() {
        let mut p = vec![TAG_RM];
        p.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
        assert!(matches!(
            decode(&frame(&p)),
            Err(KvError::Corrupt(m)) if m.starts_with("length field too long")
        ));
    }

    #[test]
    fn rm_with_trailing_bytes_is_corrupt() {
        let mut p = vec![TAG_RM];
        p.push(1);
        p.extend_from_slice(b"kx");
        assert!(matches!(
            decode(&frame(&p)),
            Err(KvError::Corrupt(m)) if m.contains("trailing bytes")
        ));
    }

    /// A `Set` has no value length, so anything after the key is value bytes.
    #[test]
    fn set_value_is_the_rest_of_the_payload() {
        let mut p = vec![TAG_SET];
        p.push(1);
        p.extend_from_slice(b"kvv");
        let record = frame(&p);
        let (cmd, _) = decode(&record).unwrap();
        assert_eq!(
            cmd,
            Command::Set {
                key: b"k",
                value: b"vv",
            }
        );
    }

    /// A key that fits one byte must not pay for two.
    #[test]
    fn key_len_varint_width_grows_with_the_key() {
        for (key_len, len_bytes) in [(0, 1), (127, 1), (128, 2), (16_383, 2), (16_384, 3)] {
            let key = vec![b'k'; key_len];
            let mut buf = Vec::new();
            encode(&Command::Rm { key: &key }, &mut buf).unwrap();
            // header + tag + key_len + key, and `Rm` carries nothing else.
            assert_eq!(buf.len(), HEADER_LEN + 1 + len_bytes + key_len);
        }
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

    /// Keys and values up to 32 arbitrary bytes: empty, ASCII, and non-UTF-8 cases.
    fn any_bytes() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(any::<u8>(), 0..32)
    }

    proptest! {
        #[test]
        fn encode_decode_round_trip(key in any_bytes(), value in any_bytes()) {
            let mut buf = Vec::new();
            for cmd in [
                Command::Set {
                    key: &key,
                    value: &value,
                },
                Command::Rm { key: &key },
            ] {
                encode(&cmd, &mut buf).unwrap();
                let (decoded, consumed) = decode(&buf).unwrap();
                prop_assert_eq!(&decoded, &cmd);
                prop_assert_eq!(consumed, buf.len());
            }
        }

        /// A longer record encoded first must not leave bytes behind for the next one.
        #[test]
        fn reused_buffer_matches_a_fresh_one(
            key in any_bytes(),
            value in any_bytes(),
            other in any_bytes(),
        ) {
            let mut reused = Vec::new();
            encode(
                &Command::Set {
                    key: &other,
                    value: &other,
                },
                &mut reused,
            )
            .unwrap();
            encode(
                &Command::Set {
                    key: &key,
                    value: &value,
                },
                &mut reused,
            )
            .unwrap();

            let mut fresh = Vec::new();
            encode(
                &Command::Set {
                    key: &key,
                    value: &value,
                },
                &mut fresh,
            )
            .unwrap();

            prop_assert_eq!(reused, fresh);
        }

        /// Hostile edits of a valid record are rejected, never a panic.
        #[test]
        fn mutated_record_is_rejected_not_panicked(
            key in any_bytes(),
            value in any_bytes(),
            at in 0usize..64,
            mask in any::<u8>(),
            cut in 0usize..64,
        ) {
            let mut buf = Vec::new();
            encode(&Command::Set { key: &key, value: &value }, &mut buf).unwrap();

            let mut flipped = buf.clone();
            let at = at % flipped.len();
            flipped[at] ^= mask.max(1);
            let _ = decode(&flipped);

            let cut = cut % buf.len();
            let _ = decode(&buf[..cut]);
        }

        /// Recomputing the CRC is what lets arbitrary bytes reach the parser at all.
        /// With a random `expected_crc` every case would stop at `ChecksumMismatch`.
        #[test]
        fn arbitrary_payload_never_panics(payload in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_payload(&payload, crc32fast::hash(&payload));
        }
    }

    /// The index adds these sizes up instead of reading records back, so they have
    /// to agree with `encode` at every varint width.
    #[test]
    fn record_len_matches_encode() {
        for (key_len, value_len) in [(0, 0), (1, 1), (127, 0), (128, 300), (16383, 1), (16384, 2)] {
            let key = vec![b'k'; key_len];
            let value = vec![b'v'; value_len];

            let mut set = Vec::new();
            encode(
                &Command::Set {
                    key: &key,
                    value: &value,
                },
                &mut set,
            )
            .unwrap();
            assert_eq!(set.len(), record_len(key_len, value_len));
            assert_eq!(&set[value_offset(0, key_len) as usize..], &value[..]);

            let mut rm = Vec::new();
            encode(&Command::Rm { key: &key }, &mut rm).unwrap();
            assert_eq!(rm.len(), record_len(key_len, 0));
        }
    }
}
