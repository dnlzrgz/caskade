use std::{fs::OpenOptions, io::Write};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

use caskade::{
    error::KvError,
    kv::{KvStore, LOG_FILE},
};

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

// Should print the value and exit 0.
#[test]
fn get_prints_value() {
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
        .args(["get", "key1"])
        .assert()
        .success()
        .stdout("value1\n")
        .stderr(predicate::str::is_empty());
}

// A missing key is an error on stderr with a non-zero exit, for `get` and `rm`.
#[test]
fn missing_key_fails() {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    for cmd in ["get", "rm"] {
        Command::cargo_bin("caskade")
            .unwrap()
            .current_dir(temp_dir.path())
            .args([cmd, "nope"])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr("caskade: key not found\n");
    }
}

// Should report the torn tail it truncated on open, singular and plural.
#[test]
fn reports_dropped_tail() {
    for (junk, unit) in [(1, "byte"), (3, "bytes")] {
        let temp_dir = TempDir::new().expect("unable to create temp working dir");
        Command::cargo_bin("caskade")
            .unwrap()
            .current_dir(temp_dir.path())
            .args(["set", "key1", "value1"])
            .assert()
            .success();
        let torn = vec![0xAA; junk];
        OpenOptions::new()
            .append(true)
            .open(temp_dir.path().join(LOG_FILE))
            .unwrap()
            .write_all(&torn)
            .unwrap();

        Command::cargo_bin("caskade")
            .unwrap()
            .current_dir(temp_dir.path())
            .args(["get", "key1"])
            .assert()
            .success()
            .stdout("value1\n")
            .stderr(format!(
                "caskade: dropped {junk} {unit} of torn tail from {LOG_FILE}\n"
            ));
    }
}

// Should address the directory given by `--db` instead of the current one.
#[test]
fn db_option_addresses_another_store() {
    let cwd = TempDir::new().expect("unable to create temp working dir");
    let store_dir = TempDir::new().expect("unable to create temp store dir");

    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(cwd.path())
        .arg("--db")
        .arg(store_dir.path())
        .args(["set", "key1", "value1"])
        .assert()
        .success();

    let store = KvStore::open(store_dir.path()).unwrap();
    assert_eq!(store.get("key1").unwrap(), "value1");
    assert!(!cwd.path().join(LOG_FILE).exists());
}
