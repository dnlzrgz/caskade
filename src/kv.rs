use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
};

use crate::{
    error::{KvError::KeyNotFound, Result},
    record::{self, Command},
};

/// The `KvStore` stores string key/value pairs.
pub struct KvStore {
    // directory for the log and other data.
    path: PathBuf,
    // Lives as long as the store.
    file: File,
    // store's current state, held in memory.
    data: HashMap<String, String>,
    // Reused scratch buffer for encoding.
    buf: Vec<u8>,
}

const LOG_FILE: &str = "caskade.log";

impl KvStore {
    /// Opens a `KvStore` with the given path.
    pub fn open(path: impl Into<PathBuf>) -> Result<KvStore> {
        let path = path.into();
        fs::create_dir_all(&path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path.join(LOG_FILE))?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            path,
            file,
            data: HashMap::new(),
            buf: Vec::new(),
        })
    }

    fn append(&mut self, cmd: &Command) -> Result<()> {
        record::encode(cmd, &mut self.buf)?;
        self.file.write_all(&self.buf)?;
        self.file.flush()?;
        Ok(())
    }

    /// Sets the value of a string key to a string. If the key already exists, the
    /// previous value will be overwritten.
    pub fn set(&mut self, key: String, value: String) -> Result<()> {
        self.append(&Command::Set {
            key: key.clone(),
            value: value.clone(),
        })?;
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
        if !self.data.contains_key(&key) {
            return Err(KeyNotFound);
        }

        self.append(&Command::Rm { key: key.clone() })?;
        self.data.remove(&key);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn set_appends_one_decodable_record() -> Result<()> {
        let dir = TempDir::new().unwrap();
        let mut store = KvStore::open(dir.path())?;
        store.set("key1".into(), "value1".into())?;

        let bytes = fs::read(dir.path().join(LOG_FILE)).unwrap();
        let (cmd, consumed) = record::decode(&bytes)?;
        assert_eq!(
            cmd,
            Command::Set {
                key: "key1".into(),
                value: "value1".into(),
            }
        );
        assert_eq!(consumed, bytes.len());

        Ok(())
    }
}
