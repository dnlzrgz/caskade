use tempfile::TempDir;

use caskade::{
    error::{KvError, Result},
    kv::KvStore,
};

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

// Should return `None` for non-existent key.
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
