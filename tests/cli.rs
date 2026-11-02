use assert_cmd::Command;
use tempfile::TempDir;

use caskade::{error::KvError, kv::KvStore};

// Should persist a write made with `--sync`.
#[test]
fn set_sync_persists() {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["set", "--sync", "key1", "value1"])
        .assert()
        .success();

    let store = KvStore::open(temp_dir.path()).unwrap();
    assert_eq!(store.get("key1").unwrap(), "value1");
}

// Should remove a key with `--sync`.
#[test]
fn rm_sync_removes() {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["set", "key1", "value1"])
        .assert()
        .success();
    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["rm", "--sync", "key1"])
        .assert()
        .success();

    let store = KvStore::open(temp_dir.path()).unwrap();
    assert!(matches!(store.get("key1"), Err(KvError::KeyNotFound)));
}
