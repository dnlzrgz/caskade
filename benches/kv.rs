use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use caskade::kv::{Durability, KvStore};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use tempfile::TempDir;

/// Replay logs are sized at one million records.
const RECORDS: usize = 1_000_000;
/// Records written per measured append iteration.
const APPEND_BATCH: usize = 10_000;
/// Records per iteration when the policy syncs.
const SYNC_APPEND_BATCH: usize = 500;
/// Distinct keys a get benchmark cycles through, spread over the whole key space.
const GET_SAMPLES: usize = 100_000;

/// Builds a log once by calling `set` into a temp dir.
fn build_log(records: usize, key_space: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut store = KvStore::open(dir.path()).unwrap();
    for i in 0..records {
        store
            .set(format!("key{}", i % key_space), format!("value{i}"))
            .unwrap();
    }

    dir
}

fn bench_replay(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay");
    group.sample_size(20);
    group.throughput(Throughput::Elements(RECORDS as u64));

    // `bounded` measures replay of a log compaction.
    for (name, key_space) in [("distinct", RECORDS * 9 / 10), ("bounded", 100)] {
        let dir = build_log(RECORDS, key_space);
        let path = dir.path().to_path_buf();

        group.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let store = KvStore::open(black_box(path.as_path())).unwrap();
                    elapsed += start.elapsed();
                    drop(store);
                }
                elapsed
            });
        });

        drop(dir);
    }

    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("get");
    group.sample_size(50);

    let key_space = RECORDS * 9 / 10;
    let dir = build_log(RECORDS, key_space);
    let store = KvStore::open(dir.path()).unwrap();
    let keys: Vec<String> = (0..GET_SAMPLES)
        .map(|i| format!("key{}", i * key_space / GET_SAMPLES))
        .collect();
    let mut next = 0;

    group.bench_function("hit", |b| {
        b.iter(|| {
            let key = &keys[next % keys.len()];
            next += 1;
            black_box(store.get(key.as_bytes()).unwrap());
        });
    });

    group.finish();
}

fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("append");
    group.sample_size(20);
    group.throughput(Throughput::Elements(APPEND_BATCH as u64));

    group.bench_function("set", |b| {
        b.iter_batched(
            || {
                let dir = TempDir::new().unwrap();
                let store = KvStore::open(dir.path()).unwrap();
                let pairs = (0..APPEND_BATCH)
                    .map(|i| (format!("key{i}"), format!("value{i}")))
                    .collect();
                (dir, store, pairs)
            },
            |(dir, mut store, pairs): (TempDir, KvStore, Vec<(String, String)>)| {
                for (key, value) in pairs {
                    store.set(key, value).unwrap();
                }
                black_box((dir, store))
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn bench_append_durability(c: &mut Criterion) {
    let mut group = c.benchmark_group("append_durability");
    group.sample_size(10);

    let policies = [
        ("never", Durability::Never, APPEND_BATCH),
        (
            "periodic_100_writes",
            Durability::Periodic {
                writes: 100,
                interval: Duration::from_secs(1),
            },
            SYNC_APPEND_BATCH,
        ),
        ("always", Durability::Always, SYNC_APPEND_BATCH),
    ];

    for (name, durability, records) in policies {
        group.throughput(Throughput::Elements(records as u64));

        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let dir = TempDir::new().unwrap();
                    let mut store = KvStore::open(dir.path()).unwrap();
                    store.set_durability(durability);
                    let pairs = (0..records)
                        .map(|i| (format!("key{i}"), format!("value{i}")))
                        .collect();
                    (dir, store, pairs)
                },
                |(dir, mut store, pairs): (TempDir, KvStore, Vec<(String, String)>)| {
                    for (key, value) in pairs {
                        store.set(key, value).unwrap();
                    }
                    black_box((dir, store))
                },
                BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

fn bench_compaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("compaction");
    group.sample_size(10);
    group.throughput(Throughput::Elements((RECORDS * 9 / 10) as u64));

    let dir = build_log(RECORDS, RECORDS * 9 / 10);
    let mut store = KvStore::open(dir.path()).unwrap();
    // Every iteration rewrites all live records into a new segment, so this is the
    // pause a merge costs on the write path; it does not shrink as the log ages.
    group.bench_function("merge", |b| {
        b.iter(|| store.compact().unwrap());
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_replay,
    bench_get,
    bench_append,
    bench_append_durability,
    bench_compaction
);
criterion_main!(benches);
