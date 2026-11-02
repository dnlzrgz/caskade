use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Write},
    path::PathBuf,
};

use crate::{
    error::{
        KvError::{self, KeyNotFound},
        Result,
    },
    record::{self, Command, Header},
};

const READ_BUF_CAPACITY: usize = 64 * 1024;

/// The `KvStore` stores string key/value pairs.
pub struct KvStore {
    // directory for the log and other data.
    path: PathBuf,
    // Lives as long as the store.
    file: File,
    // store's current state, held in memory.
    data: HashMap<String, String>,
    // Reused scratch buffer for encoding.
    buf: Vec<u8>,
}

const LOG_FILE: &str = "caskade.log";

impl KvStore {
    /// Opens a `KvStore` with the given path, replaying the log into memory.
    pub fn open(path: impl Into<PathBuf>) -> Result<KvStore> {
        let path = path.into();
        fs::create_dir_all(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path.join(LOG_FILE))?;

        let mut data = HashMap::new();
        replay(&file, &mut data)?;

        Ok(Self {
            path,
            file,
            data,
            buf: Vec::new(),
        })
    }

    fn append(&mut self, cmd: &Command<'_>) -> Result<()> {
        record::encode(cmd, &mut self.buf)?;
        self.file.write_all(&self.buf)?;
        self.file.flush()?;
        Ok(())
    }

    /// Sets the value of a string key to a string. If the key already exists, the
    /// previous value will be overwritten.
    pub fn set(&mut self, key: String, value: String) -> Result<()> {
        self.append(&Command::Set {
            key: &key,
            value: &value,
        })?;
        self.data.insert(key, value);

        Ok(())
    }

    /// Gets the string value of a given string key. Returns `KeyNotFound` if the
    /// given key doesn't exist.
    pub fn get(&self, key: &str) -> Result<String> {
        self.data.get(key).cloned().ok_or(KeyNotFound)
    }

    /// Removes a given key.
    pub fn remove(&mut self, key: &str) -> Result<()> {
        if !self.data.contains_key(key) {
            return Err(KeyNotFound);
        }

        self.append(&Command::Rm { key })?;
        self.data.remove(key);

        Ok(())
    }
}

/// Replays the whole log into memory. Right now any error aborts the whole operation.
fn replay(reader: impl Read, data: &mut HashMap<String, String>) -> Result<()> {
    let mut reader = BufReader::with_capacity(READ_BUF_CAPACITY, reader);
    let mut header_buf = [0u8; Header::SIZE];
    let mut payload = Vec::new();

    loop {
        let n = read_up_to(&mut reader, &mut header_buf)?;
        if n == 0 {
            return Ok(());
        }
        if n < Header::SIZE {
            return Err(KvError::Corrupt(format!("truncated header: {n} bytes")));
        }
        let header = Header::decode(&header_buf)?;

        payload.clear();
        let got = reader
            .by_ref()
            .take(header.payload_len as u64)
            .read_to_end(&mut payload)?;
        if got < header.payload_len {
            return Err(KvError::Corrupt(format!(
                "truncated payload: need {} bytes, have {got}",
                header.payload_len
            )));
        }

        apply(data, record::decode_payload(&payload, header.crc)?);
    }
}

/// Reads until `buf` is full or EOF.
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }

    Ok(filled)
}

/// Applies one command to the in-memory state, allocs only when a new key has to
/// be inserted.
fn apply(data: &mut HashMap<String, String>, cmd: Command<'_>) {
    match cmd {
        Command::Set { key, value } => match data.get_mut(key) {
            Some(slot) => {
                slot.clear();
                slot.push_str(&value);
            }
            None => {
                data.insert(key.to_owned(), value.to_owned());
            }
        },
        Command::Rm { key } => {
            data.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn log_bytes(cmds: &[Command<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        for cmd in cmds {
            record::encode(cmd, &mut buf).unwrap();
            out.extend_from_slice(&buf);
        }
        out
    }

    fn run(reader: impl Read) -> Result<HashMap<String, String>> {
        let mut data = HashMap::new();
        replay(reader, &mut data)?;
        Ok(data)
    }

    #[test]
    fn set_appends_one_decodable_record() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set("key1".into(), "value1".into())?;

        let bytes = fs::read(dir.path().join(LOG_FILE)).unwrap();
        let (cmd, consumed) = record::decode(&bytes)?;
        assert_eq!(
            cmd,
            Command::Set {
                key: "key1".into(),
                value: "value1".into(),
            }
        );
        assert_eq!(consumed, bytes.len());

        Ok(())
    }

    #[test]
    fn replay_empty_input_gives_empty_map() -> Result<()> {
        assert!(run(Cursor::new(Vec::new()))?.is_empty());
        Ok(())
    }

    #[test]
    fn replay_applies_commands_in_order() -> Result<()> {
        let bytes = log_bytes(&[
            Command::Set {
                key: "a",
                value: "1",
            },
            Command::Set {
                key: "b",
                value: "2",
            },
            Command::Set {
                key: "a",
                value: "3",
            },
            Command::Rm { key: "b" },
        ]);

        let data = run(Cursor::new(bytes))?;
        assert_eq!(data.len(), 1);
        assert_eq!(data["a"], "3");

        Ok(())
    }

    #[test]
    fn replay_log_larger_than_read_buffer() -> Result<()> {
        let mut bytes = Vec::new();
        let mut buf = Vec::new();
        for i in 0..5000 {
            let (k, v) = (format!("key{i}"), format!("value{i}"));
            record::encode(&Command::Set { key: &k, value: &v }, &mut buf).unwrap();
            bytes.extend_from_slice(&buf);
        }
        assert!(bytes.len() > READ_BUF_CAPACITY);

        let data = run(Cursor::new(bytes))?;
        assert_eq!(data.len(), 5000);
        assert_eq!(data["key0"], "value0");
        assert_eq!(data["key4999"], "value4999");

        Ok(())
    }

    #[test]
    fn replay_partial_header_is_corrupt() {
        let bytes = log_bytes(&[Command::Set {
            key: "a",
            value: "1",
        }]);

        // 3 stray bytes after one good record, then 3 bytes with nothing before them.
        let mut tail = bytes.clone();
        tail.extend_from_slice(&bytes[..3]);
        for input in [tail, bytes[..3].to_vec()] {
            assert!(matches!(
                run(Cursor::new(input)),
                Err(KvError::Corrupt(m)) if m.starts_with("truncated header")
            ));
        }
    }

    #[test]
    fn replay_partial_payload_is_corrupt() {
        let mut bytes = log_bytes(&[Command::Set {
            key: "a",
            value: "1",
        }]);
        bytes.pop();

        assert!(matches!(
            run(Cursor::new(bytes)),
            Err(KvError::Corrupt(m)) if m.starts_with("truncated payload")
        ));
    }

    #[test]
    fn replay_bad_record_mid_log_is_corrupt() {
        let mut bytes = log_bytes(&[
            Command::Set {
                key: "a",
                value: "1",
            },
            Command::Set {
                key: "b",
                value: "2",
            },
        ]);
        bytes[Header::SIZE] ^= 0x01;

        assert!(matches!(
            run(Cursor::new(bytes)),
            Err(KvError::Corrupt(m)) if m.starts_with("checksum mismatch")
        ));
    }
}
