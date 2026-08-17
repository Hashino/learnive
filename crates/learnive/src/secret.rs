//! Secret storage (§12) — API keys are never kept in a central DB.
//!
//! **File-first** (the user's choice): a local, gitignored, `0600` JSON file under
//! the data dir — portable, works headless and in the hosted endgame (§15). The
//! OS keychain is a future variant behind the same facade (`SecretStore`), so a
//! desktop build can upgrade storage without touching call sites (§12 keychain).
//!
//! Keys are read from here by `build_ai` when no environment override is set.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Swappable secret store (same enum-facade idiom as `ai::Provider` / `Source`).
pub enum SecretStore {
    /// Local `0600` JSON file — the default, portable everywhere.
    File(FileStore),
    // Future: Keyring(KeyringStore) — OS keychain on desktop, same interface.
}

impl SecretStore {
    /// Opens the default (file) store under the data directory.
    pub fn open(data_dir: impl AsRef<Path>) -> Self {
        SecretStore::File(FileStore::new(data_dir.as_ref().join("secrets.json")))
    }

    /// Reads a secret by name, if present.
    pub fn get(&self, name: &str) -> Option<String> {
        match self {
            SecretStore::File(s) => s.get(name),
        }
    }

    /// Stores (or replaces) a secret.
    pub fn set(&self, name: &str, value: &str) -> io::Result<()> {
        match self {
            SecretStore::File(s) => s.set(name, value),
        }
    }

    /// Removes a secret if present.
    pub fn delete(&self, name: &str) -> io::Result<()> {
        match self {
            SecretStore::File(s) => s.delete(name),
        }
    }
}

/// A flat `name → secret` JSON file, written with owner-only permissions.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn read_all(&self) -> BTreeMap<String, String> {
        fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_all(&self, map: &BTreeMap<String, String>) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(map)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // Atomic write (tmp + rename), then lock down permissions to owner-only.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;
        set_owner_only(&tmp)?;
        fs::rename(&tmp, &self.path)?;
        set_owner_only(&self.path)?;
        Ok(())
    }

    fn get(&self, name: &str) -> Option<String> {
        self.read_all().remove(name).filter(|v| !v.is_empty())
    }

    fn set(&self, name: &str, value: &str) -> io::Result<()> {
        let mut map = self.read_all();
        map.insert(name.to_string(), value.to_string());
        self.write_all(&map)
    }

    fn delete(&self, name: &str) -> io::Result<()> {
        let mut map = self.read_all();
        if map.remove(name).is_some() {
            self.write_all(&map)?;
        }
        Ok(())
    }
}

/// Restricts a file to owner read/write (`0600`) on Unix; a no-op elsewhere.
pub(crate) fn set_owner_only(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_delete_roundtrip() {
        let dir = std::env::temp_dir().join(format!("learnive-secret-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::open(&dir);

        assert_eq!(store.get("openrouter"), None);
        store.set("openrouter", "sk-or-abc").unwrap();
        assert_eq!(store.get("openrouter").as_deref(), Some("sk-or-abc"));

        // Overwrite.
        store.set("openrouter", "sk-or-xyz").unwrap();
        assert_eq!(store.get("openrouter").as_deref(), Some("sk-or-xyz"));

        store.delete("openrouter").unwrap();
        assert_eq!(store.get("openrouter"), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            store.set("k", "v").unwrap();
            let mode = fs::metadata(dir.join("secrets.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "secrets file is owner-only");
        }

        fs::remove_dir_all(&dir).ok();
    }
}
