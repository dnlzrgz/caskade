use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufReader, Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    error::{
        KvError::{self, KeyNotFound},
        Result,
    },
    record::{self, Command, Header},
};

pub const LOG_FILE: &str = "caskade.log";
const READ_BUF_CAPACITY: usize = 64 * 1024;

/// How often an append is followed by `File::sync_data`.
#[derive(Debug, Clone, Copy, Default)]
pub enum Durability {
    /// Sync after every append (slowest). An `Ok` means the bytes are on disk.
    Always,
    /// Sync when either limit trips. Pending appends are also synced when the store
    /// is dropped.
    Periodic { writes: u32, interval: Duration },
    /// Never sync.
    #[default]
    Never,
}

/// The `KvStore` stores string key/value pairs.
pub struct KvStore {
    // directory for the log and other data.
    _path: PathBuf,
    // Lives as long as the store.
    file: File,
    // store's current state, held in memory.
    data: HashMap<String, String>,
    // Reused scratch buffer for encoding.
    buf: Vec<u8>,
    // Byte length of the log that replayed cleanly (where next append lands).
    len: u64,
    // Bytes of a torn tail dropped when the store was opened.
    dropped_bytes: u64,
    durability: Durability,
    // Appends since the last sync; only used by `Durability::Periodic`.
    unsynced_appends: u32,
    last_sync: Instant,
    // Set when a write or sync failed: store must be reopened.
    poisoned: bool,
}

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
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => KvError::Locked,
            TryLockError::Error(e) => e.into(),
        })?;

        let mut data = HashMap::new();
        let len = replay(&file, &mut data)?;
        let file_len = file.metadata()?.len();
        let dropped_bytes = file_len - len;
        if dropped_bytes > 0 {
            file.set_len(len)?;
        }

        Ok(Self {
            _path: path,
            file,
            data,
            buf: Vec::new(),
            len,
            dropped_bytes,
            durability: Durability::default(),
            unsynced_appends: 0,
            last_sync: Instant::now(),
            poisoned: false,
        })
    }

    /// Bytes of a torn tail dropped when the store was opened.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// Changes the durability policy.
    pub fn set_durability(&mut self, durability: Durability) {
        self.durability = durability;
        self.unsynced_appends = 0;
        self.last_sync = Instant::now();
    }

    fn append(&mut self, cmd: &Command<'_>) -> Result<()> {
        if self.poisoned {
            return Err(KvError::Poisoned);
        }

        record::encode(cmd, &mut self.buf)?;
        let start = self.len;
        if let Err(e) = self.file.write_all(&self.buf) {
            // A half-written record would put every later append after garbage, so
            // cut the log back to its last good record.
            if self.file.set_len(start).is_err() {
                self.poisoned = true;
            }
            return Err(e.into());
        }
        self.len += self.buf.len() as u64;
        self.sync_if_due()?;

        Ok(())
    }

    /// Applies the durability policy after a record has been written.
    fn sync_if_due(&mut self) -> Result<()> {
        let due = match self.durability {
            Durability::Always => true,
            Durability::Never => false,
            Durability::Periodic { writes, interval } => {
                self.unsynced_appends += 1;
                self.unsynced_appends >= writes || self.last_sync.elapsed() >= interval
            }
        };
        if !due {
            return Ok(());
        }

        self.sync()
    }

    /// Forces the log to disk now, whatever the durability policy is.
    pub fn sync(&mut self) -> Result<()> {
        if let Err(e) = self.file.sync_data() {
            self.poisoned = true;
            return Err(e.into());
        }
        self.unsynced_appends = 0;
        self.last_sync = Instant::now();

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

impl Drop for KvStore {
    fn drop(&mut self) {
        // A `Periodic` store would otherwise never sync when the process appends
        // once and exits.
        if self.unsynced_appends > 0 {
            let _ = self.file.sync_data();
        }
    }
}

/// Replays the whole log into memory.
fn replay(reader: impl Read, data: &mut HashMap<String, String>) -> Result<u64> {
    let mut reader = BufReader::with_capacity(READ_BUF_CAPACITY, reader);
    let mut header_buf = [0u8; Header::SIZE];
    let mut payload = Vec::new();
    let mut offset = 0;

    loop {
        let n = read_up_to(&mut reader, &mut header_buf)?;
        if n == 0 {
            return Ok(offset);
        }
        if n < Header::SIZE {
            return Ok(offset); // torn header
        }
        let header = match Header::decode(&header_buf) {
            Ok(header) => header,
            Err(e) => {
                if rest_is_zero(&mut reader)? {
                    return Ok(offset);
                }
                return Err(e);
            }
        };

        payload.clear();
        let got = reader
            .by_ref()
            .take(header.payload_len as u64)
            .read_to_end(&mut payload)?;
        if got < header.payload_len {
            return Ok(offset);
        }

        match record::decode_payload(&payload, header.crc) {
            Ok(cmd) => apply(data, cmd),
            Err(KvError::ChecksumMismatch) => {
                if !rest_is_zero(&mut reader)? {
                    return Err(KvError::ChecksumMismatch);
                }
                return Ok(offset);
            }
            Err(e) => return Err(e),
        }

        offset += header.record_len() as u64;
    }
}

/// True when only zero bytes remain, which is what some filesystems leave behind
/// after a crash.
fn rest_is_zero(reader: &mut impl Read) -> io::Result<bool> {
    let mut chunk = [0u8; 4096];

    loop {
        let n = read_up_to(reader, &mut chunk)?;
        if n == 0 {
            return Ok(true);
        }
        if chunk[..n].iter().any(|&byte| byte != 0) {
            return Ok(false);
        }
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
                slot.push_str(value);
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

    fn run(reader: impl Read) -> Result<(HashMap<String, String>, u64)> {
        let mut data = HashMap::new();
        let len = replay(reader, &mut data)?;
        Ok((data, len))
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
                key: "key1",
                value: "value1",
            }
        );
        assert_eq!(consumed, bytes.len());

        Ok(())
    }

    #[test]
    fn replay_empty_input_gives_empty_map() -> Result<()> {
        let (data, len) = run(Cursor::new(Vec::new()))?;
        assert!(data.is_empty());
        assert_eq!(len, 0);

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
        let total = bytes.len() as u64;

        let (data, len) = run(Cursor::new(bytes))?;
        assert_eq!(data.len(), 1);
        assert_eq!(data["a"], "3");
        assert_eq!(len, total);

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
        let total = bytes.len() as u64;

        let (data, len) = run(Cursor::new(bytes))?;
        assert_eq!(data.len(), 5000);
        assert_eq!(data["key0"], "value0");
        assert_eq!(data["key4999"], "value4999");
        assert_eq!(len, total);

        Ok(())
    }

    #[test]
    fn replay_partial_header_recovers() -> Result<()> {
        let bytes = log_bytes(&[Command::Set {
            key: "a",
            value: "1",
        }]);
        let good_len = bytes.len() as u64;

        // 3 stray bytes after one good record, then 3 bytes with nothing before them.
        let mut tail = bytes.clone();
        tail.extend_from_slice(&bytes[..3]);

        let (data, len) = run(Cursor::new(tail))?;
        assert_eq!(data["a"], "1");
        assert_eq!(len, good_len);

        let (data, len) = run(Cursor::new(bytes[..3].to_vec()))?;
        assert!(data.is_empty());
        assert_eq!(len, 0);

        Ok(())
    }

    #[test]
    fn replay_partial_payload_recovers() -> Result<()> {
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
        let good_len = log_bytes(&[Command::Set {
            key: "a",
            value: "1",
        }])
        .len() as u64;
        bytes.pop();

        let (data, len) = run(Cursor::new(bytes))?;
        assert_eq!(data.len(), 1);
        assert_eq!(data["a"], "1");
        assert_eq!(len, good_len);

        Ok(())
    }

    #[test]
    fn replay_corrupt_final_record_recovers() -> Result<()> {
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
        let good_len = log_bytes(&[Command::Set {
            key: "a",
            value: "1",
        }])
        .len() as u64;
        *bytes.last_mut().unwrap() ^= 0x01;

        let (data, len) = run(Cursor::new(bytes))?;
        assert_eq!(data.len(), 1);
        assert_eq!(data["a"], "1");
        assert_eq!(len, good_len);

        Ok(())
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
            Err(KvError::ChecksumMismatch)
        ));
    }

    #[test]
    fn replay_damaged_header_mid_log_is_corrupt() {
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
        // Inflate the first record's length; the record's own tail is still intact.
        bytes[0] ^= 0x01;

        assert!(matches!(
            run(Cursor::new(bytes)),
            Err(KvError::Corrupt(m)) if m.starts_with("header checksum mismatch")
        ));
    }

    #[test]
    fn replay_valid_checksum_with_unknown_tag_is_corrupt() {
        let mut bytes = log_bytes(&[Command::Set {
            key: "a",
            value: "1",
        }]);
        bytes.extend_from_slice(&record::frame(&[0x02]));

        assert!(matches!(
            run(Cursor::new(bytes)),
            Err(KvError::Corrupt(m)) if m.starts_with("unknown tag")
        ));
    }

    #[test]
    fn replay_zero_filled_tail_recovers() -> Result<()> {
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
        let good_len = bytes.len() as u64;
        bytes.extend_from_slice(&[0u8; 64]);

        let (data, len) = run(Cursor::new(bytes))?;
        assert_eq!(data.len(), 2);
        assert_eq!(len, good_len);

        Ok(())
    }

    #[test]
    fn failed_append_poisons_the_store() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set("a".to_owned(), "1".to_owned())?;
        let good_len = store.len;

        store.file = File::open(dir.path().join(LOG_FILE))?;

        assert!(matches!(
            store.set("b".to_owned(), "2".to_owned()),
            Err(KvError::Io(_))
        ));
        assert!(store.poisoned);
        assert!(matches!(store.get("b"), Err(KeyNotFound)));
        assert_eq!(
            fs::metadata(dir.path().join(LOG_FILE)).unwrap().len(),
            good_len
        );
        assert!(matches!(
            store.set("c".to_owned(), "3".to_owned()),
            Err(KvError::Poisoned)
        ));

        Ok(())
    }

    #[test]
    fn periodic_durability_syncs_on_schedule() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set_durability(Durability::Periodic {
            writes: 2,
            interval: Duration::from_secs(3600),
        });

        store.set("a".to_owned(), "1".to_owned())?;
        assert_eq!(store.unsynced_appends, 1);
        store.set("b".to_owned(), "2".to_owned())?;
        assert_eq!(store.unsynced_appends, 0);

        Ok(())
    }

    #[test]
    fn sync_forces_pending_appends() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set_durability(Durability::Periodic {
            writes: 100,
            interval: Duration::from_secs(3600),
        });

        store.set("a".to_owned(), "1".to_owned())?;
        assert_eq!(store.unsynced_appends, 1);
        store.sync()?;
        assert_eq!(store.unsynced_appends, 0);

        Ok(())
    }

    #[test]
    fn always_durability_persists() -> Result<()> {
        let dir = TempDir::new().unwrap();
        {
            let mut store = KvStore::open(dir.path())?;
            store.set_durability(Durability::Always);
            store.set("a".to_owned(), "1".to_owned())?;
        }

        let store = KvStore::open(dir.path())?;
        assert_eq!(store.get("a")?, "1");

        Ok(())
    }
}
