//! The credential store: OS keyring behind a small trait, with
//! `StoreUnavailable` → `PromptPerSession` as a first-class outcome.
//!
//! PLAN.md §7 seam (b): the "unavailable OS credential store" fault
//! is injected here — [`MockCredentialStore`] fails on demand, and
//! the store layer (saved_connections) reacts by signaling
//! prompt-per-session and writing NOTHING to disk.

use keyring::Entry;

/// The keyring service name for all Redis GPUI credentials. Usernames
/// (conn ids) namespace the entries.
const SERVICE: &str = "redis-gpui";

/// How a credential operation resolved.
///
/// `StoreUnavailable` is NOT an app error: the caller switches to
/// per-session prompting (PRD acceptance 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialOutcome<T> {
    /// The operation succeeded.
    Ok(T),
    /// The OS credential store is unavailable (absent/locked/unsuited
    /// platform). The caller must prompt per session and store
    /// nothing (PLAN's PromptPerSession).
    StoreUnavailable,
    /// No credential saved for this connection (first use, or deleted).
    NotSaved,
    /// A real error that is NOT store-unavailability (e.g. a
    /// platform failure mid-write). Surfaced as an app error.
    Failure(String),
}

/// The credential store seam.
pub trait CredentialStore: Send + Sync {
    /// Save the secret for a connection (id = stable connection id).
    fn save(&self, conn_id: &str, secret: &str) -> CredentialOutcome<()>;
    /// Load the secret for a connection.
    fn load(&self, conn_id: &str) -> CredentialOutcome<String>;
    /// Delete the secret for a connection (connection removed).
    fn delete(&self, conn_id: &str) -> CredentialOutcome<()>;
}

/// The real OS keyring store (macOS Keychain / Windows Credential
/// Manager / Linux Secret Service, via the `keyring` crate).
pub struct KeyringStore;

impl CredentialStore for KeyringStore {
    fn save(&self, conn_id: &str, secret: &str) -> CredentialOutcome<()> {
        let entry = match entry_for(conn_id) {
            Ok(e) => e,
            // NoDefaultStore / platform-invalid: the store is absent.
            Err(_) => return CredentialOutcome::StoreUnavailable,
        };
        match entry.set_password(secret) {
            Ok(()) => CredentialOutcome::Ok(()),
            Err(e) if store_unavailable(&e) => CredentialOutcome::StoreUnavailable,
            Err(e) => CredentialOutcome::Failure(format!("{e}")),
        }
    }

    fn load(&self, conn_id: &str) -> CredentialOutcome<String> {
        let entry = match entry_for(conn_id) {
            Ok(e) => e,
            Err(_) => return CredentialOutcome::StoreUnavailable,
        };
        match entry.get_password() {
            Ok(s) => CredentialOutcome::Ok(s),
            Err(keyring::Error::NoEntry) => CredentialOutcome::NotSaved,
            Err(e) if store_unavailable(&e) => CredentialOutcome::StoreUnavailable,
            Err(e) => CredentialOutcome::Failure(format!("{e}")),
        }
    }

    fn delete(&self, conn_id: &str) -> CredentialOutcome<()> {
        let entry = match entry_for(conn_id) {
            Ok(e) => e,
            Err(_) => return CredentialOutcome::StoreUnavailable,
        };
        match entry.delete_credential() {
            Ok(()) => CredentialOutcome::Ok(()),
            Err(keyring::Error::NoEntry) => CredentialOutcome::Ok(()),
            Err(e) if store_unavailable(&e) => CredentialOutcome::StoreUnavailable,
            Err(e) => CredentialOutcome::Failure(format!("{e}")),
        }
    }
}

fn entry_for(conn_id: &str) -> Result<Entry, keyring::Error> {
    Entry::new(SERVICE, conn_id)
}

/// Classify keyring errors: unavailable-store vs real failure.
/// Store-unavailable: the store can't be initialized, accessed, or
/// doesn't exist on this platform — the designed degrade path.
fn store_unavailable(e: &keyring::Error) -> bool {
    match e {
        keyring::Error::NoDefaultStore | keyring::Error::NoStorageAccess(_) => true,
        keyring::Error::Invalid(param, _) => param == "platform",
        _ => false,
    }
}

/// The fault seam (PLAN §7 (b)): a mock store that can be made to
/// simulate an absent keyring.
pub struct MockCredentialStore {
    /// When true, every operation reports `StoreUnavailable`.
    pub unavailable: bool,
    /// In-memory backing for the Ok paths.
    pub entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl MockCredentialStore {
    /// A working mock (saves/loads from memory).
    pub fn working() -> Self {
        Self {
            unavailable: false,
            entries: Default::default(),
        }
    }

    /// The keyring-absent mock: everything reports `StoreUnavailable`.
    pub fn unavailable() -> Self {
        Self {
            unavailable: true,
            entries: Default::default(),
        }
    }
}

impl CredentialStore for MockCredentialStore {
    fn save(&self, conn_id: &str, secret: &str) -> CredentialOutcome<()> {
        if self.unavailable {
            return CredentialOutcome::StoreUnavailable;
        }
        self.entries
            .lock()
            .expect("mock store lock")
            .insert(conn_id.to_string(), secret.to_string());
        CredentialOutcome::Ok(())
    }

    fn load(&self, conn_id: &str) -> CredentialOutcome<String> {
        if self.unavailable {
            return CredentialOutcome::StoreUnavailable;
        }
        match self.entries.lock().expect("mock store lock").get(conn_id) {
            Some(s) => CredentialOutcome::Ok(s.clone()),
            None => CredentialOutcome::NotSaved,
        }
    }

    fn delete(&self, conn_id: &str) -> CredentialOutcome<()> {
        if self.unavailable {
            return CredentialOutcome::StoreUnavailable;
        }
        self.entries
            .lock()
            .expect("mock store lock")
            .remove(conn_id);
        CredentialOutcome::Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_working_round_trip() {
        let m = MockCredentialStore::working();
        assert!(matches!(
            m.save("conn-1", "hunter2"),
            CredentialOutcome::Ok(())
        ));
        assert_eq!(m.load("conn-1"), CredentialOutcome::Ok("hunter2".into()));
        assert_eq!(m.load("conn-2"), CredentialOutcome::NotSaved);
        assert!(matches!(m.delete("conn-1"), CredentialOutcome::Ok(())));
        assert_eq!(m.load("conn-1"), CredentialOutcome::NotSaved);
    }

    #[test]
    fn mock_unavailable_never_stores() {
        let m = MockCredentialStore::unavailable();
        assert_eq!(m.save("c", "s"), CredentialOutcome::StoreUnavailable);
        assert_eq!(m.load("c"), CredentialOutcome::StoreUnavailable);
        assert_eq!(m.delete("c"), CredentialOutcome::StoreUnavailable);
        // nothing was written
        assert!(m.entries.lock().unwrap().is_empty());
    }

    // ---------- real OS keyring (macOS Keychain on this machine) ----------
    // These pass when the Keychain is reachable; when unavailable they
    // REPORT a skip (eprintln) and still pass the not-run assertion —
    // honesty: never fake a keyring success.

    fn keychain_available() -> bool {
        // A probe entry tells us whether the store initializes.
        Entry::new(SERVICE, "redis-gpui-probe").is_ok()
    }

    #[test]
    fn real_keyring_round_trip_or_report_skip() {
        if !keychain_available() {
            eprintln!("SKIP: OS keyring unavailable; not run (not faked)");
            return;
        }
        let store = KeyringStore;
        let id = "redis-gpui-test-roundtrip";
        let secret = format!("probe-{}", std::process::id());
        match store.save(id, &secret) {
            CredentialOutcome::Ok(()) => {}
            CredentialOutcome::StoreUnavailable => {
                eprintln!("SKIP: keyring init ok but store unavailable; not run");
                return;
            }
            CredentialOutcome::Failure(e) => panic!("keyring save failed: {e}"),
            CredentialOutcome::NotSaved => unreachable!(),
        }
        assert_eq!(store.load(id), CredentialOutcome::Ok(secret.clone()));
        assert!(matches!(store.delete(id), CredentialOutcome::Ok(())));
        // gone after delete
        assert_eq!(store.load(id), CredentialOutcome::NotSaved);
    }

    #[test]
    fn real_keyring_absent_entry_is_not_saved() {
        if !keychain_available() {
            eprintln!("SKIP: OS keyring unavailable; not run (not faked)");
            return;
        }
        let store = KeyringStore;
        let id = format!("redis-gpui-test-never-{}", std::process::id());
        assert_eq!(store.load(&id), CredentialOutcome::NotSaved);
    }
}
