use std::{collections::HashMap, path::PathBuf};

use crate::error::{KvError::KeyNotFound, Result};

/// The `KvStore` stores string key/value pairs.
pub struct KvStore {
    // directory for the log and other data.
    path: PathBuf,
    // store's current state, held in memory.
    data: HashMap<String, String>,
}

impl KvStore {
    /// Opens a `KvStore` with the given path.
    pub fn open(path: impl Into<PathBuf>) -> Result<KvStore> {
        Ok(Self {
            path: path.into(),
            data: HashMap::new(),
        })
    }

    /// Sets the value of a string key to a string. If the key already exists, the
    /// previous value will be overwritten.
    pub fn set(&mut self, key: String, value: String) -> Result<()> {
        self.data.insert(key, value);
        Ok(())
    }

    /// Gets the string value of a given string key. Returns `KeyNotFound` if the
    /// given key doesn't exist.
    pub fn get(&mut self, key: String) -> Result<String> {
        self.data.get(&key).cloned().ok_or(KeyNotFound)
    }

    /// Removes a given key.
    pub fn remove(&mut self, key: String) -> Result<()> {
        self.data.remove(&key).ok_or(KeyNotFound)?;
        Ok(())
    }
}
