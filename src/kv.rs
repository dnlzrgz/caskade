use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use std::os::unix::fs::FileExt;

use rustc_hash::FxHashMap;

use crate::{
    error::{KvError, Result},
    record::{self, Command, Header},
};

/// Prefix of the log files.
pub const LOG_FILE: &str = "caskade.log";
/// Advisory lock taken for the life of a store. It is never renamed, so compaction
/// doesn't drop the lock the way renaming a data file would.
pub const LOCK_FILE: &str = "caskade.lock";
const READ_BUF_CAPACITY: usize = 64 * 1024;

/// Tunables for a store.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// A segment is sealed once it reaches this size.
    pub segment_max: u64,
    /// Stale bytes worth reclaiming.
    pub compact_min_stale: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // A gigabyte is 16 segments, a terabyte about 16k.
            segment_max: 64 << 20,
            compact_min_stale: 1 << 20,
        }
    }
}

/// Where the newest value of a key is added.
#[derive(Clone, Copy)]
struct Entry {
    /// Offset of the value bytes inside the segment.
    value_pos: u64,
    /// Length of the value. Record's length follows from it and the key.
    value_len: u32,
    /// Segment holding the record.
    file_id: u32,
}

type Map = FxHashMap<Vec<u8>, Entry>;

/// Name of the segment with `id` without the directory.
pub fn segment_file(id: u32) -> String {
    format!("{LOG_FILE}.{id:08}")
}

/// Name of a merged segment. The suffix marks every id below `id` as obsolete. This
/// makes deleting the old files (and tombstones) safe.
fn merged_file(id: u32) -> String {
    format!("{}.m", segment_file(id))
}

fn tmp_file(id: u32) -> String {
    format!("{}.tmp", segment_file(id))
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Segment,
    Merged,
    Tmp,
}

/// Parses `caskade.log.<id>[.m|.tmp]`. Anything else is ignored.
fn parse_name(name: &str) -> Option<(u32, Kind)> {
    let rest = name.strip_prefix(LOG_FILE)?.strip_prefix('.')?;
    let (digits, suffix) = rest.split_at_checked(8)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = digits.parse().ok()?;
    let kind = match suffix {
        "" => Kind::Segment,
        ".m" => Kind::Merged,
        ".tmp" => Kind::Tmp,
        _ => return None,
    };

    Some((id, kind))
}

fn segment_path(dir: &Path, id: u32, kind: Kind) -> PathBuf {
    dir.join(match kind {
        Kind::Segment => segment_file(id),
        Kind::Merged => merged_file(id),
        Kind::Tmp => tmp_file(id),
    })
}

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

/// The `KvStore` stores byte keys and values.
pub struct KvStore {
    // directory for the log and other data.
    path: PathBuf,
    // Held for the life of the store.
    _lock: File,
    // One handle per live segment.
    files: BTreeMap<u32, File>,
    // Live values by key.
    index: Map,
    // Highest segment id; appends go there.
    active: u32,
    // Bytes in the active segment, i.e. where the next append lands.
    active_len: u64,
    // Bytes in all segments together.
    total_len: u64,
    // Bytes no key points at any more.
    stale: u64,
    // Tunables, copied from `Options` at open.
    segment_max: u64,
    compact_min_stale: u64,
    // Stale bytes that must build up again before another merge is attempted after
    // one failed; `u64::MAX` means stop (corruption does not heal itself).
    compact_retry_at: u64,
    next_id: u32,
    // Reused scratch buffer for encoding.
    buf: Vec<u8>,
    // Bytes of a torn tail dropped when the store was opened.
    dropped_bytes: u64,
    durability: Durability,
    // Appends since the last sync; only used by `Durability::Periodic`.
    unsynced_appends: u32,
    last_sync: Instant,
    // Set when a write or sync failed: store must be reopened.
    poisoned: bool,
    // Fails the merge right after its rename, so the poisoning path is testable.
    #[cfg(test)]
    fail_after_rename: bool,
}

impl KvStore {
    /// Opens a `KvStore` with the given path, replaying every segment into memory.
    pub fn open(path: impl Into<PathBuf>) -> Result<KvStore> {
        Self::open_with(path, Options::default())
    }

    /// Opens a `KvStore` with the given path and tunables; `open` uses the defaults.
    pub fn open_with(path: impl Into<PathBuf>, options: Options) -> Result<KvStore> {
        let path = path.into();
        fs::create_dir_all(&path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join(LOCK_FILE))?;
        lock.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => KvError::Locked,
            TryLockError::Error(e) => e.into(),
        })?;

        let mut dropped_bytes = 0;
        let mut segments = scan(&path)?;
        adopt_legacy_log(&path, &mut segments)?;
        drop_torn_creation(&path, &mut segments, &mut dropped_bytes)?;
        drop_superseded(&path, &mut segments);
        check_contiguous(&segments)?;

        if segments.is_empty() {
            // The replay loop below opens it again.
            create_segment(&path, 0)?;
            segments.insert(0, Kind::Segment);
        }

        let last = *segments.keys().next_back().expect("a store has a segment");
        let mut index = Map::default();
        let mut stale = 0;
        let mut total_len = 0;
        let mut active_len = 0;
        let mut files = BTreeMap::new();
        for (&id, &kind) in &segments {
            let file = OpenOptions::new()
                .read(true)
                .append(true)
                .open(segment_path(&path, id, kind))?;
            let good = replay(&file, id, &mut index, &mut stale)?;
            let size = file.metadata()?.len();
            if good < size {
                // Only the segment that was being appended to can end mid-record.
                if id != last {
                    return Err(KvError::Corrupt(format!(
                        "segment {id} ends in the middle of a record"
                    )));
                }
                file.set_len(good)?;
                dropped_bytes += size - good;
            }
            total_len += good;
            if id == last {
                active_len = good;
            }
            files.insert(id, file);
        }

        Ok(Self {
            path,
            _lock: lock,
            files,
            index,
            active: last,
            active_len,
            total_len,
            stale,
            segment_max: options.segment_max,
            compact_min_stale: options.compact_min_stale,
            compact_retry_at: 0,
            next_id: last + 1,
            buf: Vec::new(),
            dropped_bytes,
            durability: Durability::default(),
            unsynced_appends: 0,
            last_sync: Instant::now(),
            poisoned: false,
            #[cfg(test)]
            fail_after_rename: false,
        })
    }

    /// Bytes of a torn tail dropped when the store was opened.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// Bytes in the log that no key points at any more.
    pub fn stale_bytes(&self) -> u64 {
        self.stale
    }

    /// Number of live keys.
    pub fn key_count(&self) -> usize {
        self.index.len()
    }

    /// Number of segment files the log is spread over.
    pub fn segment_count(&self) -> usize {
        self.files.len()
    }

    /// Bytes in every segment together, stale records included.
    pub fn log_bytes(&self) -> u64 {
        self.total_len
    }

    /// Changes the durability policy.
    pub fn set_durability(&mut self, durability: Durability) {
        self.durability = durability;
        self.unsynced_appends = 0;
        self.last_sync = Instant::now();
    }

    /// Appends one record to the active segment and returns where it starts.
    fn append(&mut self, cmd: &Command<'_>) -> Result<u64> {
        if self.poisoned {
            return Err(KvError::Poisoned);
        }
        record::encode(cmd, &mut self.buf)?;
        if self.active_len >= self.segment_max {
            self.rotate()?;
        }
        let start = self.active_len;
        let len = self.buf.len() as u64;
        let active = self.active;
        let file = self
            .files
            .get_mut(&active)
            .ok_or_else(|| KvError::Corrupt(format!("segment {active} is missing")))?;
        if let Err(e) = file.write_all(&self.buf) {
            // A half-written record would put every later append after garbage, so
            // cut the segment back to its last good record.
            if file.set_len(start).is_err() {
                self.poisoned = true;
            }
            return Err(e.into());
        }
        self.active_len += len;
        self.total_len += len;
        self.sync_if_due()?;

        Ok(start)
    }

    /// Starts a new empty segment and makes it active.
    fn rotate(&mut self) -> Result<()> {
        // Seal the old segment before a higher id exists: `open` only tolerates a
        // torn tail in the highest id, so an unsynced older segment would make the
        // entire store unopenable after a crash.
        let active = self.active;
        let sealed = self
            .files
            .get(&active)
            .ok_or_else(|| KvError::Corrupt(format!("segment {active} is missing")))?
            .sync_data();
        if let Err(e) = sealed {
            self.poisoned = true;
            return Err(e.into());
        }

        let id = self.next_id;
        let file = match create_segment(&self.path, id) {
            Ok(file) => file,
            Err(e) => {
                // Half-created segment would make every next rotation fail with
                // `AlreadyExists`; remove it, and poison if it cannot be removed.
                let path = self.path.join(segment_file(id));
                if fs::remove_file(&path).is_err() && path.exists() {
                    self.poisoned = true;
                }
                return Err(e);
            }
        };
        self.files.insert(id, file);
        self.active = id;
        self.active_len = record::FILE_HEADER_LEN as u64;
        self.total_len += self.active_len;
        self.next_id += 1;

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
        let active = self.active;
        let result = self
            .files
            .get_mut(&active)
            .ok_or_else(|| KvError::Corrupt(format!("segment {active} is missing")))?
            .sync_data();
        if let Err(e) = result {
            self.poisoned = true;
            return Err(e.into());
        }
        self.unsynced_appends = 0;
        self.last_sync = Instant::now();

        Ok(())
    }

    /// Sets the value of a byte key to a byte value. If the key already exists, the
    /// previous value will be overwritten.
    pub fn set(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Result<()> {
        let (key, value) = (key.into(), value.into());
        let pos = self.append(&Command::Set {
            key: &key,
            value: &value,
        })?;

        let key_len = key.len();
        let entry = Entry {
            value_pos: record::value_offset(pos, key_len),
            value_len: value.len() as u32,
            file_id: self.active,
        };
        if let Some(old) = self.index.insert(key, entry) {
            self.stale += record::record_len(key_len, old.value_len as usize) as u64;
        }
        self.maybe_compact();

        Ok(())
    }

    /// Gets the value of a given key. Returns `KeyNotFound` if the given key
    /// doesn't exist.
    pub fn get(&self, key: &[u8]) -> Result<Vec<u8>> {
        let entry = self.index.get(key).ok_or(KvError::KeyNotFound)?;
        self.read_value(entry)
    }

    /// Reads one value out of the segment that holds it. A positioned read keeps
    /// the shared handle's cursor intact so concurrent `get`s cannot collide.
    fn read_value(&self, entry: &Entry) -> Result<Vec<u8>> {
        let file = self
            .files
            .get(&entry.file_id)
            .ok_or_else(|| KvError::Corrupt(format!("segment {} is missing", entry.file_id)))?;
        let mut value = vec![0u8; entry.value_len as usize];
        file.read_exact_at(&mut value, entry.value_pos)?;

        Ok(value)
    }

    /// Removes a given key.
    pub fn remove(&mut self, key: &[u8]) -> Result<()> {
        let Some(old) = self.index.get(key).copied() else {
            return Err(KvError::KeyNotFound);
        };
        // Both the old value and the tombstone go stale.
        let stale = record::record_len(key.len(), old.value_len as usize) as u64
            + record::record_len(key.len(), 0) as u64;

        self.append(&Command::Rm { key })?;
        self.stale += stale;
        self.index.remove(key);
        self.maybe_compact();

        Ok(())
    }

    /// Compacts the log when the bytes nothing points at have grown enough to
    /// make it worth the copy.
    fn maybe_compact(&mut self) {
        if self.stale < self.compact_min_stale
            || self.stale < self.compact_retry_at
            || self.stale * 2 < self.total_len
        {
            return;
        }

        match self.compact() {
            Ok(()) => self.compact_retry_at = 0,
            Err(e) => {
                self.compact_retry_at = match e {
                    KvError::ChecksumMismatch | KvError::Corrupt(_) => u64::MAX,
                    _ => self.stale.saturating_add(self.compact_min_stale),
                };
            }
        }
    }

    /// Rewrites the log with only the live records and deletes the old segments.
    pub fn compact(&mut self) -> Result<()> {
        if self.poisoned {
            return Err(KvError::Poisoned);
        }

        let id = self.next_id;
        let tmp = self.path.join(tmp_file(id));
        if let Err(e) = self.write_merged(&tmp) {
            // The old segments are untouched so the store stays usable.
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }

        if let Err(e) = fs::rename(&tmp, self.path.join(merged_file(id))) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }

        // From here the merged file is the log's successor. Any failure has to stop
        // this store from appending to the old segments.
        #[cfg(test)]
        if self.fail_after_rename {
            self.fail_after_rename = false;
            self.poisoned = true;
            return Err(KvError::Io(io::Error::other(
                "injected failure after the merge rename",
            )));
        }

        let rebuilt: Result<(File, Map, u64)> = (|| {
            // Make the rename itself durable before the old files go.
            sync_dir(&self.path)?;

            let merged = OpenOptions::new()
                .read(true)
                .append(true)
                .open(self.path.join(merged_file(id)))?;
            let mut index = Map::default();
            let mut stale = 0;
            let len = replay(&merged, id, &mut index, &mut stale)?;

            if stale != 0 || len != merged.metadata()?.len() {
                return Err(KvError::Corrupt(
                    "merged log does not replay cleanly".to_owned(),
                ));
            }
            Ok((merged, index, len))
        })();

        let (merged, index, len) = match rebuilt {
            Ok(rebuilt) => rebuilt,
            Err(e) => {
                self.poisoned = true;
                return Err(e);
            }
        };

        let old: Vec<u32> = self.files.keys().copied().collect();
        self.index = index;
        // Replacing the map drops the old handles which Windows needs before the
        // files can be unlinked.
        self.files = BTreeMap::from([(id, merged)]);
        self.active = id;
        self.active_len = len;
        self.total_len = len;
        self.stale = 0;
        self.compact_retry_at = 0;
        self.next_id = id + 1;
        self.unsynced_appends = 0;
        self.last_sync = Instant::now();

        for old_id in old {
            let _ = fs::remove_file(self.path.join(segment_file(old_id)));
            let _ = fs::remove_file(self.path.join(merged_file(old_id)));
        }

        Ok(())
    }

    /// Writes every live record into a fresh segment, verifying each one on the way.
    fn write_merged(&self, tmp: &Path) -> Result<()> {
        // A crash during an earlier merge could have claimed this id already.
        let _ = fs::remove_file(tmp);
        let out = OpenOptions::new().append(true).create_new(true).open(tmp)?;
        let mut out = BufWriter::with_capacity(READ_BUF_CAPACITY, out);
        out.write_all(&record::file_header())?;

        // One sorted list of live value offsets per segment. Index order is arbitrary;
        let mut live: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
        for entry in self.index.values() {
            live.entry(entry.file_id).or_default().push(entry.value_pos);
        }

        for offsets in live.values_mut() {
            offsets.sort_unstable();
        }

        let mut head = [0u8; Header::SIZE];
        let mut payload = Vec::new();
        for (&id, offsets) in &live {
            let mut file = self
                .files
                .get(&id)
                .ok_or_else(|| KvError::Corrupt(format!("segment {id} is missing")))?;

            // Appends left the cursor at the end; start from the first record.
            file.seek(SeekFrom::Start(record::FILE_HEADER_LEN as u64))?;
            let mut offset = record::FILE_HEADER_LEN as u64;
            let mut next = 0;
            loop {
                let got = read_up_to(&mut file, &mut head)?;
                if got == 0 {
                    break;
                }
                if got < Header::SIZE {
                    return Err(KvError::Corrupt(format!(
                        "segment {id} ends in the middle of a record header"
                    )));
                }

                let header = Header::decode(&head)?;
                payload.resize(header.payload_len, 0);

                let got = read_up_to(&mut file, payload.as_mut_slice())?;
                if got < header.payload_len {
                    return Err(KvError::Corrupt(format!(
                        "segment {id} ends in the middle of a record"
                    )));
                }

                let cmd = record::decode_payload(&payload, header.crc)?;
                let value_pos = record::value_offset(offset, cmd.key_len());
                if offsets.get(next) == Some(&value_pos) {
                    next += 1;
                    out.write_all(&head)?;
                    out.write_all(&payload)?;
                }

                offset += header.record_len() as u64;
            }

            if next != offsets.len() {
                return Err(KvError::Corrupt(format!(
                    "the index points past the end of segment {id}"
                )));
            }
        }

        out.flush()?;
        // Contents must be on disk before the rename makes them the log.
        out.get_ref().sync_all()?;

        Ok(())
    }
}

impl Drop for KvStore {
    fn drop(&mut self) {
        // A `Periodic` store would otherwise never sync when the process appends
        // once and exits.
        if self.unsynced_appends > 0 {
            let _ = self.sync();
        }
    }
}

/// Creates a segment holding just its file header.
fn create_segment(dir: &Path, id: u32) -> Result<File> {
    let path = dir.join(segment_file(id));
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&path)?;

    file.write_all(&record::file_header())?;
    sync_dir(dir)?;

    Ok(file)
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;

    Ok(())
}

/// Collects the segments in `dir`. Deletes temp files a crash may have left behind too.
fn scan(dir: &Path) -> Result<BTreeMap<u32, Kind>> {
    let mut segments = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let name = entry.file_name();
        let Some((id, kind)) = name.to_str().and_then(parse_name) else {
            continue;
        };

        if kind == Kind::Tmp {
            // A merge that a crash cut short never reached its final name.
            let _ = fs::remove_file(entry.path());
            continue;
        }

        if segments.insert(id, kind).is_some() {
            return Err(KvError::Corrupt(format!("two segment files share id {id}")));
        }
    }

    Ok(segments)
}

/// Renames a pre-segment `caskade.log` to segment 0 so an older store stays readable.
fn adopt_legacy_log(dir: &Path, segments: &mut BTreeMap<u32, Kind>) -> Result<()> {
    let legacy = dir.join(LOG_FILE);
    if !legacy.is_file() {
        return Ok(());
    }

    if !segments.is_empty() {
        return Err(KvError::UnsupportedFormat(
            "pre-segment log sits next to segment files".to_owned(),
        ));
    }

    fs::rename(&legacy, dir.join(segment_file(0)))?;
    sync_dir(dir)?;
    segments.insert(0, Kind::Segment);

    Ok(())
}

/// A rotation that a crash cut short leaves a segment with no records; only
/// highest id can be affected.
fn drop_torn_creation(
    dir: &Path,
    segments: &mut BTreeMap<u32, Kind>,
    dropped: &mut u64,
) -> Result<()> {
    while let Some((&id, &kind)) = segments.last_key_value() {
        let path = segment_path(dir, id, kind);
        let size = fs::metadata(&path)?.len();
        if size >= record::FILE_HEADER_LEN as u64 {
            break;
        }

        // Only a segment this store created can be dropped; foreign bytes at a
        // short length are refused but not deleted.
        let bytes = fs::read(&path)?;
        let head = record::file_header();
        if bytes.len() >= head.len() || bytes.as_slice() != &head[..bytes.len()] {
            return Err(KvError::UnsupportedFormat(format!(
                "short segment {id} is not a caskade log"
            )));
        }

        fs::remove_file(&path)?;
        *dropped += size;
        segments.remove(&id);
    }

    Ok(())
}

/// Rotation numbers segments consecutively, so a hole means a file was lost or
/// removed by hand; replaying around it would silently drop the keys it held.
fn check_contiguous(segments: &BTreeMap<u32, Kind>) -> Result<()> {
    let Some(&first) = segments.keys().next() else {
        return Ok(());
    };

    let mut expected = first;
    for &id in segments.keys() {
        if id != expected {
            return Err(KvError::Corrupt(format!(
                "segment {expected} is missing from the log"
            )));
        }

        expected = expected.saturating_add(1);
    }

    Ok(())
}

/// A merged segment supersedes every id below its own, so those files can never be
/// read again and deleting them is safe even if a crash interrupts a cleanup.
fn drop_superseded(dir: &Path, segments: &mut BTreeMap<u32, Kind>) {
    let floor = segments
        .iter()
        .rev()
        .find(|&(_, &kind)| kind == Kind::Merged)
        .map(|(&id, _)| id)
        .unwrap_or(0);
    let superseded: Vec<u32> = segments.keys().copied().filter(|&id| id < floor).collect();
    for id in superseded {
        let kind = segments.remove(&id).expect("id was just listed");
        // Failed unlink is retried at the next open.
        let _ = fs::remove_file(segment_path(dir, id, kind));
    }
}

/// Replays one segment into `index`, counting the bytes it makes stale, and
/// returns where it stopped (the offset after the last good record).
fn replay<R: Read + Seek>(
    mut reader: R,
    file_id: u32,
    index: &mut Map,
    stale: &mut u64,
) -> Result<u64> {
    reader.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(READ_BUF_CAPACITY, reader);
    let mut header_buf = [0u8; Header::SIZE];
    let mut payload = Vec::new();

    let mut file_header = [0u8; record::FILE_HEADER_LEN];
    let n = read_up_to(&mut reader, &mut file_header)?;
    record::check_file_header(&file_header[..n])?;

    let mut offset = record::FILE_HEADER_LEN as u64;

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

        // Decode straight out of the read buffer.
        let buf = reader.fill_buf()?;
        let (payload_bytes, buffered) = if buf.len() >= header.payload_len {
            (&buf[..header.payload_len], true)
        } else {
            payload.clear();

            let got = reader
                .by_ref()
                .take(header.payload_len as u64)
                .read_to_end(&mut payload)?;
            if got < header.payload_len {
                return Ok(offset);
            }

            (&payload[..], false)
        };

        match record::decode_payload(payload_bytes, header.crc) {
            Ok(cmd) => {
                apply(index, stale, cmd, offset, file_id);
                if buffered {
                    reader.consume(header.payload_len);
                }
            }
            Err(KvError::ChecksumMismatch) => {
                // The record's own bytes don't count as a zero-filled tail.
                if buffered {
                    reader.consume(header.payload_len);
                }
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

/// Applies one record to the index, counting the bytes it invalidates.
fn apply(index: &mut Map, stale: &mut u64, cmd: Command<'_>, record_pos: u64, file_id: u32) {
    match cmd {
        Command::Set { key, value } => {
            let entry = Entry {
                value_pos: record::value_offset(record_pos, key.len()),
                value_len: value.len() as u32,
                file_id,
            };

            if let Some(old) = index.insert(key.to_vec(), entry) {
                *stale += record::record_len(key.len(), old.value_len as usize) as u64;
            }
        }
        Command::Rm { key } => {
            if let Some(old) = index.remove(key) {
                *stale += record::record_len(key.len(), old.value_len as usize) as u64;
            }

            *stale += record::record_len(key.len(), 0) as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn log_bytes(cmds: &[Command<'_>]) -> Vec<u8> {
        let mut out = record::file_header().to_vec();
        let mut buf = Vec::new();
        for cmd in cmds {
            record::encode(cmd, &mut buf).unwrap();
            out.extend_from_slice(&buf);
        }
        out
    }

    fn run(bytes: &[u8]) -> Result<(Map, u64)> {
        let mut data = FxHashMap::default();
        let mut stale = 0;
        let len = replay(Cursor::new(bytes), 0, &mut data, &mut stale)?;
        Ok((data, len))
    }

    fn lookup<'a>(bytes: &'a [u8], data: &Map, key: &str) -> &'a [u8] {
        let entry = data.get(key.as_bytes()).expect("key is in the index");
        &bytes[entry.value_pos as usize..][..entry.value_len as usize]
    }

    #[test]
    fn set_appends_one_decodable_record() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set("key1", "value1")?;

        let bytes = fs::read(dir.path().join(segment_file(0))).unwrap();
        let (cmd, consumed) = record::decode(&bytes[record::FILE_HEADER_LEN..])?;
        assert_eq!(
            cmd,
            Command::Set {
                key: b"key1",
                value: b"value1",
            }
        );
        assert_eq!(consumed, bytes.len() - record::FILE_HEADER_LEN);

        Ok(())
    }

    #[test]
    fn replay_header_only_gives_empty_map() -> Result<()> {
        let (data, len) = run(&record::file_header())?;
        assert!(data.is_empty());
        assert_eq!(len, record::FILE_HEADER_LEN as u64);

        Ok(())
    }

    #[test]
    fn replay_applies_commands_in_order() -> Result<()> {
        let bytes = log_bytes(&[
            Command::Set {
                key: b"a",
                value: b"1",
            },
            Command::Set {
                key: b"b",
                value: b"2",
            },
            Command::Set {
                key: b"a",
                value: b"3",
            },
            Command::Rm { key: b"b" },
        ]);
        let total = bytes.len() as u64;

        let (data, len) = run(&bytes)?;
        assert_eq!(data.len(), 1);
        assert_eq!(lookup(&bytes, &data, "a"), b"3");
        assert_eq!(len, total);

        Ok(())
    }

    #[test]
    fn replay_log_larger_than_read_buffer() -> Result<()> {
        let mut bytes = record::file_header().to_vec();
        let mut buf = Vec::new();
        for i in 0..5000 {
            let (k, v) = (format!("key{i}"), format!("value{i}"));
            record::encode(
                &Command::Set {
                    key: k.as_bytes(),
                    value: v.as_bytes(),
                },
                &mut buf,
            )
            .unwrap();
            bytes.extend_from_slice(&buf);
        }
        assert!(bytes.len() > READ_BUF_CAPACITY);
        let total = bytes.len() as u64;

        let (data, len) = run(&bytes)?;
        assert_eq!(data.len(), 5000);
        assert_eq!(lookup(&bytes, &data, "key0"), b"value0");
        assert_eq!(lookup(&bytes, &data, "key4999"), b"value4999");
        assert_eq!(len, total);

        Ok(())
    }

    #[test]
    fn replay_reads_records_across_buffer_limits() -> Result<()> {
        let mut owned: Vec<(String, Vec<u8>)> = (0..200)
            .map(|i| (format!("key{i}"), vec![b'a' + (i % 26) as u8; 1000]))
            .collect();
        owned.push(("big".to_owned(), vec![b'z'; READ_BUF_CAPACITY + 7]));
        owned.push(("last".to_owned(), b"end".to_vec()));

        let mut bytes = record::file_header().to_vec();
        let mut buf = Vec::new();
        for (key, value) in &owned {
            record::encode(
                &Command::Set {
                    key: key.as_bytes(),
                    value,
                },
                &mut buf,
            )
            .unwrap();
            bytes.extend_from_slice(&buf);
        }
        let total = bytes.len() as u64;
        assert!(total > READ_BUF_CAPACITY as u64);

        let (data, len) = run(&bytes)?;
        assert_eq!(len, total);
        for (key, value) in &owned {
            assert_eq!(lookup(&bytes, &data, key), value.as_slice(), "{key}");
        }

        Ok(())
    }

    #[test]
    fn replay_partial_header_recovers() -> Result<()> {
        let bytes = log_bytes(&[Command::Set {
            key: b"a",
            value: b"1",
        }]);
        let good_len = bytes.len() as u64;

        // A record header cut short by a crash.
        let mut tail = bytes.clone();
        tail.extend_from_slice(&[16, 0, 0]);

        let (data, len) = run(&tail)?;
        assert_eq!(lookup(&bytes, &data, "a"), b"1");
        assert_eq!(len, good_len);

        Ok(())
    }

    #[test]
    fn replay_partial_payload_recovers() -> Result<()> {
        let mut bytes = log_bytes(&[
            Command::Set {
                key: b"a",
                value: b"1",
            },
            Command::Set {
                key: b"b",
                value: b"2",
            },
        ]);
        let good_len = log_bytes(&[Command::Set {
            key: b"a",
            value: b"1",
        }])
        .len() as u64;
        bytes.pop();

        let (data, len) = run(&bytes)?;
        assert_eq!(data.len(), 1);
        assert_eq!(lookup(&bytes, &data, "a"), b"1");
        assert_eq!(len, good_len);

        Ok(())
    }

    #[test]
    fn replay_corrupt_final_record_recovers() -> Result<()> {
        let mut bytes = log_bytes(&[
            Command::Set {
                key: b"a",
                value: b"1",
            },
            Command::Set {
                key: b"b",
                value: b"2",
            },
        ]);
        let good_len = log_bytes(&[Command::Set {
            key: b"a",
            value: b"1",
        }])
        .len() as u64;
        *bytes.last_mut().unwrap() ^= 0x01;

        let (data, len) = run(&bytes)?;
        assert_eq!(data.len(), 1);
        assert_eq!(lookup(&bytes, &data, "a"), b"1");
        assert_eq!(len, good_len);

        Ok(())
    }

    #[test]
    fn replay_bad_record_mid_log_is_corrupt() {
        let mut bytes = log_bytes(&[
            Command::Set {
                key: b"a",
                value: b"1",
            },
            Command::Set {
                key: b"b",
                value: b"2",
            },
        ]);
        bytes[record::FILE_HEADER_LEN + Header::SIZE] ^= 0x01;

        assert!(matches!(run(&bytes), Err(KvError::ChecksumMismatch)));
    }

    #[test]
    fn replay_damaged_header_mid_log_is_corrupt() {
        let mut bytes = log_bytes(&[
            Command::Set {
                key: b"a",
                value: b"1",
            },
            Command::Set {
                key: b"b",
                value: b"2",
            },
        ]);
        // Inflate the first record's length; the record's own tail is still intact.
        bytes[record::FILE_HEADER_LEN] ^= 0x01;

        assert!(matches!(
            run(&bytes),
            Err(KvError::Corrupt(m)) if m.starts_with("header checksum mismatch")
        ));
    }

    #[test]
    fn replay_valid_checksum_with_unknown_tag_is_corrupt() {
        let mut bytes = log_bytes(&[Command::Set {
            key: b"a",
            value: b"1",
        }]);
        bytes.extend_from_slice(&record::frame(&[0x02]));

        assert!(matches!(
            run(&bytes),
            Err(KvError::Corrupt(m)) if m.starts_with("unknown tag")
        ));
    }

    #[test]
    fn replay_zero_filled_tail_recovers() -> Result<()> {
        let mut bytes = log_bytes(&[
            Command::Set {
                key: b"a",
                value: b"1",
            },
            Command::Set {
                key: b"b",
                value: b"2",
            },
        ]);
        let good_len = bytes.len() as u64;
        bytes.extend_from_slice(&[0u8; 64]);

        let (data, len) = run(&bytes)?;
        assert_eq!(data.len(), 2);
        assert_eq!(len, good_len);

        Ok(())
    }

    #[test]
    fn failed_append_poisons_the_store() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set("a".to_owned(), "1".to_owned())?;
        let good_len = store.active_len;

        store
            .files
            .insert(store.active, File::open(dir.path().join(segment_file(0)))?);

        assert!(matches!(
            store.set("b".to_owned(), "2".to_owned()),
            Err(KvError::Io(_))
        ));
        assert!(store.poisoned);
        assert!(matches!(store.get(b"b"), Err(KvError::KeyNotFound)));
        assert_eq!(
            fs::metadata(dir.path().join(segment_file(0)))
                .unwrap()
                .len(),
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
    fn index_entry_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<Entry>(), 16);
    }

    #[test]
    fn overwrite_and_remove_count_stale_bytes() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set("a", "1")?;
        assert_eq!(store.stale_bytes(), 0);

        let value = record::record_len(1, 1) as u64;
        let tombstone = record::record_len(1, 0) as u64;
        store.set("a", "2")?;
        assert_eq!(store.stale_bytes(), value);
        store.remove(b"a")?;
        assert_eq!(store.stale_bytes(), value * 2 + tombstone);

        // Replay has to arrive at the same count as the write path did.
        drop(store);
        let store = KvStore::open(dir.path())?;
        assert_eq!(store.stale_bytes(), value * 2 + tombstone);

        Ok(())
    }

    #[test]
    fn compaction_twice_keeps_the_index_consistent() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        for round in 0..5 {
            store.set("a", format!("v{round}"))?;
        }
        store.compact()?;
        store.set("a", "v5")?;
        store.compact()?;
        assert_eq!(store.get(b"a")?.as_slice(), b"v5");
        assert_eq!(store.stale_bytes(), 0);

        drop(store);
        let store = KvStore::open(dir.path())?;
        assert_eq!(store.get(b"a")?.as_slice(), b"v5");

        Ok(())
    }

    #[test]
    fn a_failed_merge_after_the_rename_poisons_the_store() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set("a", "1")?;
        store.set("b", "2")?;

        store.fail_after_rename = true;
        assert!(store.compact().is_err());
        assert!(store.poisoned);
        // Nothing may be appended after this: the next open deletes the segments it
        // would have gone to.
        assert!(matches!(store.set("c", "3"), Err(KvError::Poisoned)));

        // The merged file is a complete snapshot, so a reopen keeps every write.
        drop(store);
        let store = KvStore::open(dir.path())?;
        assert_eq!(store.get(b"a")?.as_slice(), b"1");
        assert_eq!(store.get(b"b")?.as_slice(), b"2");

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
        assert_eq!(store.get(b"a")?.as_slice(), b"1");

        Ok(())
    }
}
