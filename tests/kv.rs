use std::fs;

use tempfile::TempDir;

use caskade::{
    error::{KvError, Result},
    kv::{KvStore, LOG_FILE},
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
    let path = temp_dir.path().join(LOG_FILE);
    {
        let mut store = KvStore::open(temp_dir.path())?;
        store.set("key1".to_owned(), "value1".to_owned())?;
        store.set("key2".to_owned(), "value2".to_owned())?;
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
    store.set("key3".to_owned(), "value3".to_owned())?;
    drop(store);
    let store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.get(b"key3")?.as_slice(), b"value3");

    Ok(())
}

// Should refuse to open when a record before the end of the log is damaged.
#[test]
fn open_rejects_mid_log_corruption() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let path = temp_dir.path().join(LOG_FILE);
    {
        let mut store = KvStore::open(temp_dir.path())?;
        store.set("key1".to_owned(), "value1".to_owned())?;
        store.set("key2".to_owned(), "value2".to_owned())?;
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
    let path = temp_dir.path().join(LOG_FILE);
    {
        let mut store = KvStore::open(temp_dir.path())?;
        store.set("key1".to_owned(), "value1".to_owned())?;
        store.set("key2".to_owned(), "value2".to_owned())?;
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

    store.set("key1".to_owned(), "value1".to_owned())?;
    store.set("key2".to_owned(), "value2".to_owned())?;

    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");
    assert_eq!(store.get(b"key2")?.as_slice(), b"value2");

    Ok(())
}

// Should overwrite existent value.
#[test]
fn overwrite_existing_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1".to_owned(), "value1".to_owned())?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value1");
    store.set("key1".to_owned(), "value2".to_owned())?;
    assert_eq!(store.get(b"key1")?.as_slice(), b"value2");

    Ok(())
}

// Should return `KeyNotFound` for a non-existent key.
#[test]
fn get_non_existent_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1".to_owned(), "value1".to_owned())?;
    assert!(matches!(store.get(b"key2"), Err(KvError::KeyNotFound)));

    Ok(())
}

// Should remove an existing key, leaving nothing to read afterwards.
#[test]
fn remove_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1".to_owned(), "value1".to_owned())?;
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

    store.set("key1".to_owned(), "value1".to_owned())?;
    store.remove(b"key1")?;
    store.set("key1".to_owned(), "value2".to_owned())?;

    assert_eq!(store.get(b"key1")?.as_slice(), b"value2");

    Ok(())
}

// Should repair a file header write that a crash cut short, since nothing can
// follow it.
#[test]
fn open_repairs_torn_file_header() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let path = temp_dir.path().join(LOG_FILE);
    fs::write(&path, b"cask")?;

    let mut store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.dropped_bytes(), 4);
    store.set("key1".to_owned(), "value1".to_owned())?;
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
    let path = temp_dir.path().join(LOG_FILE);

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
