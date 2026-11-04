use std::{fs::OpenOptions, io::Write};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

use caskade::{
    error::KvError,
    kv::{KvStore, LOCK_FILE, segment_file},
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
    assert_eq!(store.get(b"key1").unwrap().as_slice(), b"value1");
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
    assert!(matches!(store.get(b"key1"), Err(KvError::KeyNotFound)));
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
            .open(temp_dir.path().join(segment_file(0)))
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
            .stderr(format!("caskade: dropped {junk} {unit} of torn tail\n"));
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
    assert_eq!(store.get(b"key1").unwrap().as_slice(), b"value1");
    assert!(!cwd.path().join(segment_file(0)).exists());
    assert!(!cwd.path().join(LOCK_FILE).exists());
}

// Should fail at once with the CLI message when another store holds the directory.
#[test]
fn locked_directory_fails_fast() {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let _held = KvStore::open(temp_dir.path()).unwrap();

    Command::cargo_bin("caskade")
        .unwrap()
        .arg("--db")
        .arg(temp_dir.path())
        .args(["set", "key1", "value1"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr("caskade: directory is locked by another store\n");
}

// Should print a binary value unchanged, byte for byte.
#[test]
fn get_prints_binary_value() {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    let value = vec![0xFF, 0x00, 0x80, b'\n', 0xC3];
    {
        let mut store = KvStore::open(temp_dir.path()).unwrap();
        store.set("key1", value.clone()).unwrap();
    }

    let mut expected = value;
    expected.push(b'\n');
    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["get", "key1"])
        .assert()
        .success()
        .stdout(expected);
}

// Should compact from the command line and keep the newest value.
#[test]
fn compact_subcommand_rewrites_the_log() {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    for round in 0..3 {
        let value = format!("value{round}");
        Command::cargo_bin("caskade")
            .unwrap()
            .current_dir(temp_dir.path())
            .args(["set", "key1", &value])
            .assert()
            .success();
    }

    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["compact"])
        .assert()
        .success();

    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["get", "key1"])
        .assert()
        .success()
        .stdout("value2\n");
}

// Should report the shape of the log with one line per field.
#[test]
fn stats_reports_keys_segments_and_bytes() {
    let temp_dir = TempDir::new().expect("unable to create temp working dir");
    for (key, value) in [("key1", "value1"), ("key2", "value2")] {
        Command::cargo_bin("caskade")
            .unwrap()
            .current_dir(temp_dir.path())
            .args(["set", key, value])
            .assert()
            .success();
    }
    // Removing leaves the value and the tombstone behind as stale bytes.
    Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["rm", "key2"])
        .assert()
        .success();

    let assert = Command::cargo_bin("caskade")
        .unwrap()
        .current_dir(temp_dir.path())
        .args(["stats"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    let fields: std::collections::HashMap<&str, &str> = stdout
        .lines()
        .map(|line| line.split_once(": ").expect("name: value"))
        .collect();
    assert_eq!(fields.len(), 4, "unexpected stats output: {stdout}");
    assert_eq!(fields["keys"], "1");
    assert_eq!(fields["segments"], "1");
    let log_bytes: u64 = fields["log_bytes"].parse().unwrap();
    let stale_bytes: u64 = fields["stale_bytes"].parse().unwrap();
    assert!(stale_bytes > 0 && stale_bytes < log_bytes, "{stdout}");
}
