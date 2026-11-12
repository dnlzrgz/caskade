# caskade

[![crates.io](https://img.shields.io/crates/v/caskade.svg)](https://crates.io/crates/caskade)
[![docs.rs](https://img.shields.io/docsrs/caskade)](https://docs.rs/caskade)
[![MSRV](https://img.shields.io/badge/MSRV-1.89-blue)](https://github.com/dnlzrgz/caskade)

A log-structured, persistent key/value store, inspired by [bitcask](https://github.com/basho/bitcask) and written from scratch in Rust for fun and learning.

## Motivation

I wanted to learn more about database internals and while researching the topic I found the [mini-LSM](https://skyzh.github.io/mini-lsm/00-overview.html) course. It recommends first following [PingCAP's project](https://github.com/pingcap/talent-plan/tree/master/courses/rust/projects/project-2) to build a "bitcask-style key/value store". Unfortunately, the project is a bit old, so I changed a few things, such as a custom binary format instead of JSON and `thiserror` for error handling.

After "finishing" the tutorial, I kept going:

- Corruption detection (per-record CRC plus a header checksum).
- Recovery from a torn tail after a crash.
- Configurable durability policy.
- Locking, so two processes cannot corrupt one log.
- Segment files, so the log does not have to be one ever-growing file.
- `stats`, criterion benchmarks and property testing.

The end result is still a small project: around 2,000 lines of code.

## AI usage

AI assistance was used only for research, testing, and documentation.

## Features

- Append-only log with 64 MiB segments (configurable via `Options`).
- In-memory offset index. Values stay on disk and are read on demand.
- Automatic and explicit compaction.
- Torn-tail recovery.
- Durability policies: `Always`, `Periodic`, `Never`, plus explicit `sync()`.
- Versioned on-disk format.
- CLI (`get`, `set`, `rm`, `compact`, `stats`) and a library API.

## Usage

### CLI

```console
$ cargo install caskade
$ caskade set greeting hello
$ caskade get greeting
hello
$ caskade stats
keys: 1
segments: 1
log_bytes: 39
stale_bytes: 0
```

`--db <DIR>` selects the store directory and works before or after the subcommand, for example `caskade --db /var/lib/caskade stats`.

> Writes are not fsynced by default; add `--sync` to `set` or `rm` to make that one write durable.

### Library

```rust
use caskade::error::KvError;
use caskade::kv::{Durability, KvStore};

fn main() -> Result<(), KvError> {
    let mut store = KvStore::open("db")?;
    store.set("key", "value")?;
    assert_eq!(store.get(b"key")?.as_slice(), b"value");

    store.set_durability(Durability::Always);
    store.set("durable", "write")?;
    store.sync()
}
```

## Performance

Criterion benchmarks on a 13th Gen Intel Core i7-13650HX (20 threads), Linux. Release profile with `lto = "fat"`, `codegen-units = 1`, and page cache warm. Times are criterion's point estimates and you can expect roughly +/-5% between runs.

| Benchmark | Workload | Time | Rate |
| --- | --- | --- | --- |
| `replay/distinct` | open and replay 1M records / 900k keys | 135.1 ms | 7.4M records/s |
| `replay/bounded` | open and replay the ~1 MiB log left by 1M overwrites of 100 keys | 343.2 µs | - |
| `get/hit` | one `get` over 100k keys from a 900k-key store | 412.2 ns | 2.4M gets/s |
| `append/set` | 10,000 `set`s, default `Durability::Never` | 8.09 ms | 1.24M writes/s (809 ns each) |
| `append_durability/periodic_100_writes` | 500 `set`s, fsync every 100 writes | 8.69 ms | 58k writes/s |
| `append_durability/always` | 500 `set`s, fsync per write | 569.1 ms | 878 writes/s (1.14 ms each) |
| `compaction/merge` | full merge of 900k live records | 212.1 ms | 4.24M records/s |

Reading the table:

- Replay is the number that affects startup time. It is map-bound, not
  parsing-bound: 1M records into a 900k-entry map takes 135 ms.
- A `get` is a hash lookup plus one positioned read.
- Appends cost 809 ns each with no fsync. `always` costs 1.14 ms per write on this disk, so fsync is ~99% of the write cost. `periodic` amortizes it.
- A merge rewrites the entire live set, so its cost is the pause a compaction causes: about 0.21 s per 900k live keys.

## Design decisions

**Binary log, not a serialization framework.** Records are hand-encoded: a 12-byte header (`len`, `crc`, `hcrc`), then a tag, a LEB128 key length, the key and the value (see [On-disk format](#on-disk-format)). The header checksum is the interesting part: without it, a damaged length cannot be told apart from a payload cut short by a crash, and replay would throw away the good records behind it.

**Offset index.** The map holds `key -> (segment, value offset, value length)` in a 16-byte entry. A `get` is a hash lookup and one positioned read. Memory grows with the number of keys, not the bytes stored, at the cost of a disk read per hit (412 ns instead of ~57 ns when values lived in memory).

**Segments and merge-all compaction.** The log is split into `caskade.log.<id>` files and appends go to the highest id. A merge rewrites every live record into `caskade.log.<next>.m`, fsyncs it, renames it, and only then deletes the old files. The `.m` marker means "this supersedes every lower id", which is what makes dropping tombstones safe and a crash during cleanup harmless. Merging everything is the simplest correct version, and it caps write amplification at about 2x because a merge only triggers once stale bytes reach live bytes. The price is a pause: a merge is synchronous and rewrites the whole live set.

**Failures are handled on purpose.** A failed append truncates back to the last good record. A failed merge before the rename leaves the old log untouched. A failure after the rename poisons the store instead of appending to files the next open would delete. Rotation fsyncs the segment it seals, because `open` only tolerates a torn tail in the highest id.

**Durability is not promised.** The default is `Never` (the same choice bitcask and RocksDB make): bytes reach the OS, not the disk. `Always` fsyncs every write, `Periodic` spreads the fsync cost over several writes, and `sync()` forces one. `Drop` flushes pending `Periodic` appends, so a one-write process still syncs.

**One writer, many readers.** Writes take `&mut self`, so there is one writer; reads take `&self` and use a positioned read (`pread`), never the shared file cursor, so a store is `Sync` and several threads can `get` from one. An advisory lock on `caskade.lock` keeps two processes apart: a second `open` fails at once with `KvError::Locked`, and the kernel drops the lock when the process dies.

## Limitations

- Every CLI command replays the log on open.
- No fsync by default: a power loss can lose recent writes. Use `--sync`, `Durability::Always`, or call `sync()`.
- Linux only: positioned reads and the directory fsync use the Linux std extensions.
- One store per directory, one process.
- Compaction is merge-all and synchronous: there is no background merge (yet).
- The index is in memory: ~120 B per key (152 MiB for 1M keys, 1.2 GB for 10M), so hundreds of millions of keys are out of reach.
- `set` rejects keys over 4 KiB (`MAX_KEY_LEN`). `get` and `remove` stay uncapped so a key written by an older build remains readable and deletable.
- The index uses `FxHash`, which is fast but not hash-flooding resistant: keys from an untrusted caller could be chosen to collide.
- No read cache and no per-read checksum: corruption is detected during replay and compaction, not by `get`.
- No iteration or range scans: keys are not ordered, and there is no way to list them yet.
- The CLI is UTF-8 only while the library is byte-oriented.
- One file descriptor per segment: fine at the 64 MiB default, but it may matter for very large stores.

## On-disk format

A store directory contains `caskade.lock` and one or more log files:

```text
caskade.log.<id>       segment
caskade.log.<id>.m     merged segment; supersedes every lower id
caskade.log.<id>.tmp   merge in progress; neither replayed nor trusted
```

Each segment starts with a 12-byte file header:

```text
file header := "caskade\n" (8 bytes) | version u32 LE (currently 1)
```

The header is followed by records until the segment's end:

```text
record  := len u32 LE | crc u32 LE | hcrc u32 LE | payload
payload := 0x00 | key_len varint | key | value    (Set; value is the rest)
           0x01 | key_len varint | key             (Rm)
```

- `len` is the payload length, so a record spans `12 + len` bytes.
- `crc` is CRC-32 over the payload; `hcrc` is CRC-32 over `len` and `crc`.
- `key_len` is unsigned LEB128 (1 byte below 128, at most 4 bytes). The current
  maximum payload is 64 MiB, and the writer rejects `Set` keys over 4 KiB.
- Keys and values are opaque bytes. No encoding is assumed.

Replay walks segments in ascending id and records in file order; the last record for a key wins.
