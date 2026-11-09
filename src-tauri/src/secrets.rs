use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::{AppError, AppResult};

const SERVICE: &str = "io.github.mellowforge.porthole";

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> AppResult<Option<String>>;
    fn set(&self, key: &str, value: &str) -> AppResult<()>;
    fn delete(&self, key: &str) -> AppResult<()>;
}

pub fn password_key(connection_id: &str) -> String {
    format!("connection:{connection_id}")
}

/// OS keychain-backed store (macOS Keychain via the `keyring` crate).
pub struct KeychainStore;

impl KeychainStore {
    fn entry(key: &str) -> AppResult<keyring::Entry> {
        keyring::Entry::new(SERVICE, key).map_err(secret_err)
    }
}

// Maps keyring errors without ever including secret values.
fn secret_err(e: keyring::Error) -> AppError {
    AppError::Secret(e.to_string())
}

impl SecretStore for KeychainStore {
    fn get(&self, key: &str) -> AppResult<Option<String>> {
        match Self::entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(secret_err(e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> AppResult<()> {
        Self::entry(key)?.set_password(value).map_err(secret_err)
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(secret_err(e)),
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> AppResult<Option<String>> {
        Ok(self.map.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> AppResult<()> {
        self.map.lock().unwrap().insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        self.map.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_delete_roundtrip() {
        let s = MemoryStore::default();
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
        s.delete("k").unwrap();
        assert_eq!(s.get("k").unwrap(), None);
    }

    #[test]
    fn delete_missing_is_ok() {
        MemoryStore::default().delete("nope").unwrap();
    }

    #[test]
    fn password_key_format() {
        assert_eq!(password_key("abc"), "connection:abc");
    }

    #[test]
    #[ignore = "touches the real macOS Keychain"]
    fn keychain_roundtrip() {
        let s = KeychainStore;
        let key = format!("test:{}", uuid::Uuid::new_v4());
        assert_eq!(s.get(&key).unwrap(), None);
        s.set(&key, "secret").unwrap();
        assert_eq!(s.get(&key).unwrap().as_deref(), Some("secret"));
        s.delete(&key).unwrap();
        assert_eq!(s.get(&key).unwrap(), None);
        s.delete(&key).unwrap();
    }
}
