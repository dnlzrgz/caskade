//! # caskade
//!
//! A log-structured, persistent key/value store inspired by [bitcask](https://github.com/basho/bitcask),
//! built from scratch as a learning project.
//! This is not a production database: one writer, no background threads, and
//! the API and scope of the project may change before reaching 1.0.
//!
//! ```
//! use caskade::kv::{Durability, KvStore};
//!
//! let dir = tempfile::tempdir().unwrap();
//! let mut store = KvStore::open(dir.path()).unwrap();
//!
//! store.set("hello", "world").unwrap();
//! assert_eq!(store.get(b"hello").unwrap().as_slice(), b"world");
//!
//! store.remove(b"hello").unwrap();
//! assert!(store.get(b"hello").is_err());
//!
//! // Durability is a per-store policy; `Always` fsyncs on every write.
//! store.set_durability(Durability::Always);
//! store.set("durable", "write").unwrap();
//! store.sync().unwrap();
//! ```
//!
//! ## Durability
//!
//! The default is [`Durability::Never`](kv::Durability::Never): writes reach the OS but
//! are not fsynced, so a power loss can lose recent appends.
//! [`Durability::Always`](kv::Durability::Always) syncs after every append, and
//! [`Durability::Periodic`](kv::Durability::Periodic) amortizes fsyncs across writes.
//! [`KvStore::sync`](kv::KvStore::sync) forces one whatever the policy is, and dropping
//! a store flushes pending `Periodic` appends.
//!
//! ## Concurrency and locking
//!
//! A store is single-writer and [`KvStore`](kv::KvStore) is `!Sync` on purpose: `get`
//! seeks the segment's shared file cursor, so two threads sharing one store would read
//! each other's offsets. Opening a directory takes an advisory lock on `caskade.lock`; a
//! second open fails at once with [`KvError::Locked`](crate::error::KvError::Locked).
//! Retrying is the caller's job, and the kernel releases the lock when the process
//! dies.
//!
//! ## Recovery and format
//!
//! Opening a store replays every segment into an in-memory index of
//! `key -> (segment, value offset, value length)`; values stay on disk. Records carry
//! a CRC-32 and a header checksum, and only the active (highest-id) segment may end
//! in the middle of a record: that suffix is truncated and reported by
//! [`KvStore::dropped_bytes`](kv::KvStore::dropped_bytes). Damage anywhere else is a
//! [`KvError::Corrupt`](crate::error::KvError::Corrupt) error. The on-disk format is
//! versioned and documented in the [README](https://github.com/dnlzrgz/caskade#on-disk-format).
//!
//! ## Modules
//!
//! - [`kv`]: the store, [`Options`](kv::Options) and [`Durability`](kv::Durability)
//! - [`error`]: the error type
//! - [`cli`]: `clap` types used by the `caskade` binary
#![forbid(unsafe_code)]

pub mod cli;
pub mod error;
pub mod kv;
mod record;
