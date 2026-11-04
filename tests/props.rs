use std::collections::HashMap;

use proptest::prelude::*;
use tempfile::TempDir;

use caskade::kv::{KvStore, Options};

#[derive(Debug, Clone)]
enum Op {
    Set(u8, Vec<u8>),
    Remove(u8),
    Compact,
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (any::<u8>(), prop::collection::vec(any::<u8>(), 0..24))
            .prop_map(|(key, value)| Op::Set(key, value)),
        any::<u8>().prop_map(Op::Remove),
        Just(Op::Compact),
        Just(Op::Reopen),
    ]
}

/// Eight keys, so collisions force overwrites and removals to hit live records.
fn key(k: u8) -> Vec<u8> {
    format!("key{}", k % 8).into_bytes()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn behaves_like_a_hashmap(ops in prop::collection::vec(op(), 1..96)) {
        let dir = TempDir::new().unwrap();
        // Tiny segments and a tiny stale floor, so rotation and merges happen often.
        let options = Options {
            segment_max: 384,
            compact_min_stale: 128,
        };
        let mut store = KvStore::open_with(dir.path(), options).unwrap();
        let mut model: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

        for op in ops {
            let label = format!("{op:?}");
            match op {
                Op::Set(k, v) => {
                    let k = key(k);
                    store.set(k.clone(), v.clone()).unwrap();
                    model.insert(k, v);
                }
                Op::Remove(k) => {
                    let k = key(k);
                    let expected = model.remove(&k).is_some();
                    prop_assert_eq!(store.remove(&k).is_ok(), expected, "remove {:?}", k);
                }
                Op::Compact => store.compact().unwrap(),
                Op::Reopen => {
                    drop(store);
                    store = KvStore::open_with(dir.path(), options).unwrap();
                }
            }

            for k in 0..8u8 {
                let k = key(k);
                prop_assert_eq!(
                    store.get(&k).ok(),
                    model.get(&k).cloned(),
                    "key {:?} after {}",
                    k,
                    label
                );
            }
        }
    }
}
