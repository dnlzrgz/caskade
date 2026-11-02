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
    assert_eq!(store.get("key1")?, "value1");
    assert_eq!(store.get("key2")?, "value2");
    assert_eq!(store.dropped_bytes(), 5);
    assert_eq!(fs::metadata(&path).unwrap().len(), good_len);

    // The next append must land at the truncated end, not after the torn record.
    store.set("key3".to_owned(), "value3".to_owned())?;
    drop(store);
    let store = KvStore::open(temp_dir.path())?;
    assert_eq!(store.get("key3")?, "value3");

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
    bytes[12] ^= 0x01; // the record header is 12 bytes
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
    bytes[0] ^= 0x01;
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

    assert_eq!(store.get("key1")?, "value1");
    assert_eq!(store.get("key2")?, "value2");

    Ok(())
}

// Should overwrite existent value.
#[test]
fn overwrite_existing_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1".to_owned(), "value1".to_owned())?;
    assert_eq!(store.get("key1")?, "value1");
    store.set("key1".to_owned(), "value2".to_owned())?;
    assert_eq!(store.get("key1")?, "value2");

    Ok(())
}

// Should return `KeyNotFound` for a non-existent key.
#[test]
fn get_non_existent_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1".to_owned(), "value1".to_owned())?;
    assert!(matches!(store.get("key2"), Err(KvError::KeyNotFound)));

    Ok(())
}

// Should remove an existing key, leaving nothing to read afterwards.
#[test]
fn remove_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1".to_owned(), "value1".to_owned())?;
    store.remove("key1")?;
    assert!(matches!(store.get("key1"), Err(KvError::KeyNotFound)));

    Ok(())
}

// Should return `KeyNotFound` error when removing a missing key.
#[test]
fn remove_non_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    assert!(matches!(store.remove("key1"), Err(KvError::KeyNotFound)));

    Ok(())
}

// Should store a new value for a key that was previously removed.
#[test]
fn set_after_remove() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let mut store = KvStore::open(temp_dir.path())?;

    store.set("key1".to_owned(), "value1".to_owned())?;
    store.remove("key1")?;
    store.set("key1".to_owned(), "value2".to_owned())?;

    assert_eq!(store.get("key1")?, "value2");

    Ok(())
}
