use std::{fs, path::Path};

use tempfile::TempDir;
use walkdir::WalkDir;

use caskade::{
    error::{KvError, Result},
    kv::{KvStore, LOG_FILE, Options, segment_file},
};

// Should let only one store hold a directory at a time.
#[test]
fn second_open_on_same_directory_is_locked_out() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let first = KvStore::open(temp_dir.path())?;

    assert!(matches!(
        KvStore::open(temp_dir.path()),
        Err(KvError::Locked)
    ));

    // Closing the first store releases the lock.
    drop(first);
    KvStore::open(temp_dir.path())?;

    Ok(())
}

// Should drop a record cut short at the end of the log and truncate it away.
#[test]
fn open_truncates_torn_tail() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let path = temp_dir.path().join(segment_file(0));
    {
        let mut store = KvStore::open(temp_dir.path())?;
        store.set("key1", "value1")?;
        store.set("key2", "value2")?;
    }
    let good_len = fs::metadata(&path).unwrap().len();

    // A header the crash cut off before the length was complete.
    let mut torn = fs::read(&path)?;
    torn.extend_from_slice(&[16, 0, 0, 0, 0]);
    fs::write(&path, &torn)?;

    let mut store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");
    assert_eq!(store.get(b"key2")?.as_slice(), b"value2");
    assert_eq!(store.dropped_bytes(), 5);
    assert_eq!(fs::metadata(&path).unwrap().len(), good_len);

    // The next append must land at the truncated end, not after the torn record.
    store.set("key3", "value3")?;
    drop(store);
    let store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.get(b"key3")?.as_slice(), b"value3");

    Ok(())
}

// Should refuse to open when a record before the end of the log is damaged.
#[test]
fn open_rejects_mid_log_corruption() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let path = temp_dir.path().join(segment_file(0));
    {
        let mut store = KvStore::open(temp_dir.path())?;
        store.set("key1", "value1")?;
        store.set("key2", "value2")?;
    }

    // Flip a byte of the first record's payload; its CRC no longer matches.
    let mut bytes = fs::read(&path)?;
    bytes[24] ^= 0x01; // a 12-byte file header, then a 12-byte record header
    fs::write(&path, &bytes)?;

    assert!(matches!(
        KvStore::open(temp_dir.path()),
        Err(KvError::ChecksumMismatch)
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), bytes.len() as u64);

    Ok(())
}

// Should refuse to open when a length field is damaged, rather than guess where
// the next record starts and truncate the good records behind it.
#[test]
fn open_rejects_damaged_length() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let path = temp_dir.path().join(segment_file(0));
    {
        let mut store = KvStore::open(temp_dir.path())?;
        store.set("key1", "value1")?;
        store.set("key2", "value2")?;
    }

    // A flipped length byte would otherwise read as a payload cut short by a crash.
    let mut bytes = fs::read(&path)?;
    bytes[12] ^= 0x01; // first byte after the 12-byte file header
    fs::write(&path, &bytes)?;

    assert!(matches!(
        KvStore::open(temp_dir.path()),
        Err(KvError::Corrupt(m)) if m.starts_with("header checksum mismatch")
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), bytes.len() as u64);

    Ok(())
}

// Should get previously stored value.
#[test]
fn get_stored_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1", "value1")?;
    store.set("key2", "value2")?;

    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");
    assert_eq!(store.get(b"key2")?.as_slice(), b"value2");

    Ok(())
}

// Should overwrite existing value.
#[test]
fn overwrite_existing_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1", "value1")?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");
    store.set("key1", "value2")?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value2");

    Ok(())
}

// Should return `KeyNotFound` for a non-existent key.
#[test]
fn get_non_existent_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1", "value1")?;
    assert!(matches!(store.get(b"key2"), Err(KvError::KeyNotFound)));

    Ok(())
}

// Should remove an existing key, leaving nothing to read afterwards.
#[test]
fn remove_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1", "value1")?;
    store.remove(b"key1")?;
    assert!(matches!(store.get(b"key1"), Err(KvError::KeyNotFound)));

    Ok(())
}

// Should return `KeyNotFound` error when removing a missing key.
#[test]
fn remove_non_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    assert!(matches!(store.remove(b"key1"), Err(KvError::KeyNotFound)));

    Ok(())
}

// Should store a new value for a key that was previously removed.
#[test]
fn set_after_remove() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1", "value1")?;
    store.remove(b"key1")?;
    store.set("key1", "value2")?;

    assert_eq!(store.get(b"key1")?.as_slice(), b"value2");

    Ok(())
}

// Should drop a segment that a crash cut short while it was being created: it
// holds no records, so nothing after it can exist.
#[test]
fn open_drops_a_segment_created_short_by_a_crash() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let path = temp_dir.path().join(segment_file(0));
    fs::write(&path, b"cask")?;

    let mut store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.dropped_bytes(), 4);
    store.set("key1", "value1")?;
    drop(store);

    let store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");

    Ok(())
}

// Should refuse a log this build cannot read instead of misreading its bytes, and
// leave the file alone.
#[test]
fn open_rejects_unknown_format() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let path = temp_dir.path().join(segment_file(0));

    // Another writer's file, and this format at a version we do not know.
    for bytes in [
        b"not a caskade log at all".to_vec(),
        [b"caskade\n".as_slice(), &2u32.to_le_bytes()].concat(),
    ] {
        fs::write(&path, &bytes)?;
        assert!(matches!(
            KvStore::open(temp_dir.path()),
            Err(KvError::UnsupportedFormat(_))
        ));
        assert_eq!(fs::read(&path)?, bytes);
    }

    Ok(())
}

// Should store arbitrary bytes, including ones that are not valid UTF-8, and
// replay them unchanged.
#[test]
fn stores_non_utf8_bytes() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let key = vec![0xFF, 0x00, 0xFE, b'k'];
    let value = vec![0x80, 0xC3, 0x28, 0x00, 0xFF];
    {
        let mut store = KvStore::open(temp_dir.path())?;
        store.set(key.clone(), value.clone())?;
        assert_eq!(store.get(&key)?, value);
    }

    let store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.get(&key)?, value);
    assert_eq!(store.get(b"key1").unwrap_err().to_string(), "key not found");

    Ok(())
}

/// Names of the segment files in `dir`, temp files excluded.
fn segment_files(dir: &Path) -> Vec<String> {
    let prefix = format!("{LOG_FILE}.");
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&prefix) && !name.ends_with(".tmp"))
        .collect()
}

/// Total size of every file in the store directory.
fn dir_size(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .into_iter()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}

// Should keep every live value and shrink the log when compacted explicitly.
#[test]
fn explicit_compaction_keeps_values_and_shrinks_the_log() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let mut store = KvStore::open(dir)?;
    for round in 0..200 {
        for key in 0..10 {
            store.set(format!("key{key}"), format!("value{round}"))?;
        }
    }

    let before = dir_size(dir);
    assert!(store.stale_bytes() > 0);
    store.compact()?;
    assert_eq!(store.stale_bytes(), 0);
    assert!(dir_size(dir) < before, "compaction reclaimed nothing");
    for key in 0..10 {
        assert_eq!(
            store.get(format!("key{key}").as_bytes())?.as_slice(),
            format!("value{}", 199).as_bytes()
        );
    }

    // The merged file must replay into the same state.
    drop(store);
    let store = KvStore::open(dir)?;
    assert_eq!(store.stale_bytes(), 0);
    for key in 0..10 {
        assert_eq!(
            store.get(format!("key{key}").as_bytes())?.as_slice(),
            format!("value{}", 199).as_bytes()
        );
    }

    Ok(())
}

// Should compact on its own once enough of the log is stale, so the directory
// stops growing. This is the shape of the tutorial's compaction test.
#[test]
fn log_does_not_grow_forever() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let mut store = KvStore::open(dir)?;

    let mut current_size = dir_size(dir);
    for round in 0..200 {
        for key in 0..1000 {
            store.set(format!("key{key}"), format!("{round}"))?;
        }

        let new_size = dir_size(dir);
        if new_size > current_size {
            current_size = new_size;
            continue;
        }

        // The log stopped growing, so compaction ran: the state must survive it.
        // Staleness is not zero here: the merge fires mid-round, and the rest of
        // the round overwrites records the merged file already holds.
        drop(store);
        let store = KvStore::open(dir)?;
        for key in 0..1000 {
            assert_eq!(
                store.get(format!("key{key}").as_bytes())?.as_slice(),
                round.to_string().as_bytes()
            );
        }
        return Ok(());
    }

    panic!("the log grew for 200 rounds without compacting");
}

// Should never let a deleted key come back through a segment that a merged one
// supersedes, even if a crash left that segment behind.
#[test]
fn superseded_segment_cannot_resurrect_a_removed_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let old_segment = {
        let mut store = KvStore::open(dir)?;
        store.set("ghost", "value")?;
        drop(store);
        fs::read(dir.join(segment_file(0)))?
    };

    let mut store = KvStore::open(dir)?;
    store.remove(b"ghost")?;
    store.compact()?;
    drop(store);
    assert!(!dir.join(segment_file(0)).exists());

    // Put the superseded segment back, as a crash between the rename and the
    // cleanup would.
    fs::write(dir.join(segment_file(0)), &old_segment)?;

    let store = KvStore::open(dir)?;
    assert!(matches!(store.get(b"ghost"), Err(KvError::KeyNotFound)));
    assert!(!dir.join(segment_file(0)).exists());

    Ok(())
}

// Should start a new segment once the active one reaches its limit, and keep
// reading the old one.
#[test]
fn rotation_starts_a_new_segment() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let options = Options {
        segment_max: 4096,
        ..Options::default()
    };
    let mut store = KvStore::open_with(dir, options)?;
    let value = vec![b'v'; 512];

    let mut records = 0;
    while !dir.join(segment_file(1)).exists() {
        store.set(format!("key{records}"), value.clone())?;
        records += 1;
        assert!(records < 100_000, "the segment never rotated");
    }

    assert_eq!(store.get(b"key0")?.as_slice(), value.as_slice());
    let last = format!("key{}", records - 1);
    assert_eq!(store.get(last.as_bytes())?.len(), value.len());

    drop(store);
    let store = KvStore::open_with(dir, options)?;
    assert_eq!(store.get(b"key0")?.as_slice(), value.as_slice());
    assert_eq!(store.get(last.as_bytes())?.len(), value.len());

    Ok(())
}

// Should delete a merge that a crash cut short instead of replaying it.
#[test]
fn leftover_merge_file_is_removed_on_open() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    {
        let mut store = KvStore::open(dir)?;
        store.set("key1", "value1")?;
    }
    let tmp = dir.join(format!("{LOG_FILE}.00000009.tmp"));
    fs::write(&tmp, b"half a merge, then the power went")?;

    let store = KvStore::open(dir)?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");
    assert!(!tmp.exists());

    Ok(())
}

// Should treat a torn tail in a segment that is not the active one as corruption,
// since only the active segment can have been appended to when the crash hit.
#[test]
fn open_rejects_a_torn_tail_in_an_old_segment() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    {
        let mut store = KvStore::open(dir)?;
        store.set("key1", "value1")?;
    }

    let first = dir.join(segment_file(0));
    let mut bytes = fs::read(&first)?;
    fs::write(dir.join(segment_file(1)), &bytes)?;
    bytes.extend_from_slice(&[16, 0, 0]);
    fs::write(&first, &bytes)?;

    assert!(matches!(KvStore::open(dir), Err(KvError::Corrupt(_))));

    Ok(())
}

// Should read a log written before the store was split into segments.
#[test]
fn open_adopts_a_pre_segment_log() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    {
        let mut store = KvStore::open(dir)?;
        store.set("key1", "value1")?;
    }
    fs::rename(dir.join(segment_file(0)), dir.join(LOG_FILE))?;

    let store = KvStore::open(dir)?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");
    assert!(dir.join(segment_file(0)).exists());
    assert!(!dir.join(LOG_FILE).exists());

    Ok(())
}

// Should rotate away from a merged segment that is already over the size limit,
// and keep reading the values left in it.
#[test]
fn a_merged_segment_over_the_limit_rotates_on_the_next_write() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let mut store = KvStore::open_with(
        dir,
        Options {
            segment_max: 1 << 20,
            ..Options::default()
        },
    )?;
    let value = vec![b'v'; 512];
    for key in 0..4000 {
        store.set(format!("key{key}"), value.clone())?;
    }

    store.compact()?;
    assert_eq!(segment_files(dir).len(), 1, "merge left more than one file");

    // The merged file is over 1 MiB on its own, so the next write has to rotate.
    store.set("newkey", "value")?;
    assert_eq!(segment_files(dir).len(), 2);
    assert_eq!(store.get(b"key0")?.as_slice(), value.as_slice());
    assert_eq!(store.get(b"newkey")?.as_slice(), b"value");

    drop(store);
    let store = KvStore::open(dir)?;
    assert_eq!(store.get(b"key0")?.as_slice(), value.as_slice());
    assert_eq!(
        store.get(format!("key{}", 3999).as_bytes())?.len(),
        value.len()
    );
    assert_eq!(store.get(b"newkey")?.as_slice(), b"value");

    Ok(())
}

// Should let the newest record win when a key is overwritten after a rotation.
#[test]
fn overwrite_in_a_later_segment_wins() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let options = Options {
        segment_max: 4096,
        ..Options::default()
    };
    let mut store = KvStore::open_with(dir, options)?;
    store.set("key", "old")?;

    let mut records = 0;
    while !dir.join(segment_file(1)).exists() {
        store.set(format!("fill{records}"), vec![b'f'; 256])?;
        records += 1;
        assert!(records < 100, "the segment never rotated");
    }
    store.set("key", "new")?;

    assert_eq!(store.get(b"key")?.as_slice(), b"new");
    drop(store);

    // Later ids must win on replay.
    let store = KvStore::open_with(dir, options)?;
    assert_eq!(store.get(b"key")?.as_slice(), b"new");

    Ok(())
}

// Should read keys at both varint widths, and a value larger than the read buffer,
// end to end through the index.
#[test]
fn reopen_handles_varint_key_widths_and_large_values() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let keys = [vec![b'k'; 127], vec![b'k'; 128]];
    let big = vec![b'v'; 70 * 1024];

    {
        let mut store = KvStore::open(dir)?;
        for (i, key) in keys.iter().enumerate() {
            store.set(key.clone(), vec![b'0' + i as u8; 3])?;
        }
        store.set(b"big", big.clone())?;
    }

    let store = KvStore::open(dir)?;
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(store.get(key)?, vec![b'0' + i as u8; 3], "{i}");
    }
    assert_eq!(store.get(b"big")?, big);

    Ok(())
}

// Should count stale bytes that belong to records sealed into earlier segments.
#[test]
fn stale_bytes_survive_a_rotation() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let options = Options {
        segment_max: 4096,
        ..Options::default()
    };
    let mut store = KvStore::open_with(dir, options)?;
    store.set("key", "one")?;

    let mut records = 0;
    while !dir.join(segment_file(1)).exists() {
        store.set(format!("fill{records}"), vec![b'f'; 256])?;
        records += 1;
        assert!(records < 100, "the segment never rotated");
    }
    store.set("key", "two")?;

    let stale = store.stale_bytes();
    assert!(stale > 0);
    drop(store);

    // Replay has to arrive at the same count as the write path did.
    let store = KvStore::open_with(dir, options)?;
    assert_eq!(store.stale_bytes(), stale);

    Ok(())
}

// Should refuse two files that claim the same segment id instead of replaying one
// of them at random.
#[test]
fn duplicate_segment_ids_are_refused() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    {
        let mut store = KvStore::open(dir)?;
        store.set("key1", "value1")?;
    }
    let plain = dir.join(segment_file(0));
    fs::copy(&plain, dir.join(format!("{}.m", segment_file(0))))?;

    assert!(matches!(KvStore::open(dir), Err(KvError::Corrupt(_))));

    Ok(())
}

// Should refuse a log with a segment missing from the middle, rather than replaying
// around it and silently dropping its keys.
#[test]
fn a_missing_middle_segment_is_refused() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    {
        let mut store = KvStore::open(dir)?;
        store.set("key1", "value1")?;
    }
    fs::copy(dir.join(segment_file(0)), dir.join(segment_file(2)))?;

    assert!(matches!(
        KvStore::open(dir),
        Err(KvError::Corrupt(m)) if m.contains("missing")
    ));

    Ok(())
}

// Should compact only once both the stale floor and the ratio say it is worth it.
#[test]
fn compaction_waits_for_its_threshold() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let dir = temp_dir.path();
    let options = Options {
        compact_min_stale: 200,
        ..Options::default()
    };
    let mut store = KvStore::open_with(dir, options)?;
    store.set("a", "1")?;
    store.set("b", "2")?;
    store.set("a", "2")?;

    // One stale record, well under the floor: nothing has been merged yet.
    let first = store.stale_bytes();
    assert!(first > 0 && first < 200);

    // Keep overwriting; the trigger must eventually reset the count. The loop stops
    // on the write that triggered the merge, so remember which value that was.
    let mut merged = false;
    let mut last = Vec::new();
    for round in 0..100 {
        last = format!("v{round}").into_bytes();
        store.set("a", last.clone())?;
        if store.stale_bytes() == 0 {
            merged = true;
            break;
        }
    }
    assert!(merged, "the compaction trigger never fired");
    assert_eq!(store.get(b"a")?.as_slice(), last.as_slice());

    Ok(())
}

// Should keep `get` safe on a shared store: positioned reads do not move the
// segment cursor, so two threads cannot read each other's offsets.
#[test]
fn concurrent_reads_do_not_move_the_cursor() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let options = Options {
        segment_max: 4096,
        ..Options::default()
    };
    let mut store = KvStore::open_with(temp_dir.path(), options)?;

    // Spread keys over several segments and include an empty value.
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..200)
        .map(|i| {
            let key = format!("key{i}").into_bytes();
            let value = if i % 10 == 0 {
                Vec::new()
            } else {
                vec![b'a' + (i % 26) as u8; i % 64]
            };
            (key, value)
        })
        .collect();
    for (key, value) in &pairs {
        store.set(key.clone(), value.clone())?;
    }
    assert!(store.segment_count() > 1, "the keys stayed in one segment");

    let store = &store;
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let pairs = &pairs;
            scope.spawn(move || {
                for _ in 0..50 {
                    for (key, value) in pairs {
                        assert_eq!(&store.get(key).expect("key is in the index"), value);
                    }
                }
            });
        }
    });

    Ok(())
}

// Should keep the "reads are safe from several threads" claim from drifting.
#[test]
fn store_is_sync() {
    fn assert_sync<T: Sync>() {}
    assert_sync::<KvStore>();
}
