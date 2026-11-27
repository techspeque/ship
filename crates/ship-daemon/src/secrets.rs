//! The Phase 1 secret store (ADR-0009 rule 3): one file per value under
//! `<state>/secrets/`, the directory 0700 and each file 0600, owned by the
//! user the daemon runs as (`ship` under the systemd unit). This is file
//! permissions, not encryption (`docs/secrets.md`); Phase 2 encrypts the
//! store behind the same calls (rule 4).
//!
//! A file is named by a random backend reference from the store, so a
//! value is written to a new file and only then recorded; the file it
//! replaces is removed after. A crash in between leaves an unreferenced
//! file, never a record without its value.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ship_core::SecretValue;
use ship_store::{Store, StoreError};

use crate::log::Redactor;

/// The secret store's directory under the state directory.
pub const SECRETS_DIR: &str = "secrets";
/// Mode of the directory.
pub const DIR_MODE: u32 = 0o700;
/// Mode of each value's file.
pub const FILE_MODE: u32 = 0o600;

/// The secret store of a state directory.
#[derive(Debug, Clone)]
pub struct SecretStore {
    dir: PathBuf,
}

impl SecretStore {
    /// The store under `state_dir`. Nothing is created until a value is
    /// written or [`SecretStore::ensure`] runs.
    pub fn new(state_dir: &Path) -> SecretStore {
        SecretStore {
            dir: state_dir.join(SECRETS_DIR),
        }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates the directory at 0700, or puts an existing one back to 0700.
    pub fn ensure(&self) -> io::Result<()> {
        match fs::DirBuilder::new().mode(DIR_MODE).create(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&self.dir)?;
                if !meta.is_dir() {
                    return Err(io::Error::other("the secret store is not a directory"));
                }
                if meta.permissions().mode() & 0o7777 != DIR_MODE {
                    fs::set_permissions(&self.dir, fs::Permissions::from_mode(DIR_MODE))?;
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn path(&self, backend_ref: &str) -> io::Result<PathBuf> {
        if backend_ref.is_empty()
            || !backend_ref
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a secret reference is lowercase hex",
            ));
        }
        Ok(self.dir.join(backend_ref))
    }

    /// Writes `value` to a new file named `backend_ref` (0600, created
    /// exclusively, synced).
    pub fn write(&self, backend_ref: &str, value: &SecretValue) -> io::Result<()> {
        self.ensure()?;
        let path = self.path(backend_ref)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&path)?;
        let written = file
            .write_all(value.expose().as_bytes())
            .and_then(|()| file.sync_all());
        if let Err(e) = written {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        fs::File::open(&self.dir)?.sync_all()
    }

    /// The value kept under `backend_ref`.
    pub fn read(&self, backend_ref: &str) -> io::Result<SecretValue> {
        let mut text = String::new();
        fs::File::open(self.path(backend_ref)?)?.read_to_string(&mut text)?;
        Ok(SecretValue::new(text))
    }

    /// Removes the value kept under `backend_ref`; a missing one is fine.
    pub fn remove(&self, backend_ref: &str) -> io::Result<()> {
        match fs::remove_file(self.path(backend_ref)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// At daemon start: puts the directory at 0700 and registers every stored
/// secret's value with `redactor`, so a value set before a restart is
/// still never printed. A value that cannot be read is logged by name and
/// skipped; a deployment that needs it fails then. Returns how many were
/// registered.
pub async fn register_all(
    store: &Store,
    secrets: &SecretStore,
    redactor: &Redactor,
) -> Result<usize, StoreError> {
    secrets.ensure().map_err(StoreError::Io)?;
    let mut n = 0;
    for record in store.all_secrets().await? {
        match secrets.read(&record.backend_ref) {
            Ok(value) => {
                redactor.register(&value);
                n += 1;
            }
            Err(e) => {
                tracing::error!(secret = %record.name, scope = %record.target.scope(), error = %e,
                    "a stored secret's value cannot be read");
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn values_are_written_0600_in_a_0700_directory_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path());
        let r = "0123456789abcdef0123456789abcdef";
        store.write(r, &SecretValue::new("hunter2-store")).unwrap();
        assert_eq!(mode(store.dir()), DIR_MODE);
        assert_eq!(mode(&store.dir().join(r)), FILE_MODE);
        assert_eq!(store.read(r).unwrap().expose(), "hunter2-store");
        // Never overwritten in place.
        assert!(store.write(r, &SecretValue::new("other")).is_err());
        store.remove(r).unwrap();
        store.remove(r).unwrap();
        assert!(store.read(r).is_err());
    }

    #[test]
    fn a_loosened_directory_is_tightened_and_odd_references_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path());
        store.ensure().unwrap();
        fs::set_permissions(store.dir(), fs::Permissions::from_mode(0o755)).unwrap();
        store.ensure().unwrap();
        assert_eq!(mode(store.dir()), DIR_MODE);
        for bad in ["", "../x", "ABC", "a/b", "."] {
            assert!(store.write(bad, &SecretValue::new("v")).is_err(), "{bad}");
            assert!(store.read(bad).is_err(), "{bad}");
        }
    }
}
