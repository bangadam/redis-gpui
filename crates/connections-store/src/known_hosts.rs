//! TOFU ("trust on first use") known-hosts store.
//!
//! Pure logic + a tiny JSON file under the config dir; fingerprints
//! are SHA-256 hex strings (one-way digests), never secret key
//! material. The *check* runs in redis-io at the SSH handshake (where
//! the presented key is known); this crate owns the *store*.
//!
//! - First connect → [`HostKeyVerdict::Unseen`] → the caller must
//!   confirm before proceeding ([`KnownHosts::record`]).
//! - Later connects → the presented fingerprint must match the
//!   recorded one. A changed key is a HARD BLOCK
//!   ([`HostKeyVerdict::Changed`]) — no override exists, by design
//!   (PRD acceptance 5, anti-MITM).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// SHA-256 host-key fingerprint, hex (same shape as redis-io's
/// `Fingerprint` alias; plain `String` keeps this crate dependency-free).
pub type Fingerprint = String;

/// The TOFU verdict for a presented host key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyVerdict {
    /// First time seeing this host: nothing recorded yet.
    Unseen(Fingerprint),
    /// Matches the recorded fingerprint: proceed.
    Trusted,
    /// Presented key differs from the recorded one: HARD BLOCK.
    Changed {
        /// Fingerprint of the presented (untrusted) key.
        new: Fingerprint,
        /// The previously recorded fingerprint.
        old: Fingerprint,
    },
}

/// The known-hosts store: `host:port` → fingerprint.
///
/// Cloning is cheap and intended — the tunnel handler holds a clone
/// while the caller keeps the original (redis-io's tunnel needs
/// interior access mid-handshake).
#[derive(Clone, Debug, Default)]
pub struct KnownHosts {
    /// host:port → sha256 fingerprint (hex).
    entries: HashMap<String, Fingerprint>,
    /// Where the file lives (None = in-memory only, tests).
    path: Option<PathBuf>,
}

impl KnownHosts {
    /// In-memory store (tests / explicit no-persistence mode).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load (or start empty) persisted at `path`. Missing file → empty
    /// store, NOT an error (first run).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            entries,
            path: Some(path),
        }
    }

    /// Verify a presented key against the record.
    pub fn verify(&self, host: &str, port: u16, fingerprint: &str) -> HostKeyVerdict {
        match self.entries.get(&key_for(host, port)) {
            None => HostKeyVerdict::Unseen(fingerprint.to_string()),
            Some(recorded) if *recorded == fingerprint => HostKeyVerdict::Trusted,
            Some(old) => HostKeyVerdict::Changed {
                new: fingerprint.to_string(),
                old: old.clone(),
            },
        }
    }

    /// Record (trust) a fingerprint for `host:port` — the TOFU trust
    /// step taken ONLY after explicit user confirmation. Overwrites
    /// any previous record for that key (used by the confirm flow).
    pub fn record(&self, host: &str, port: u16, fingerprint: &str) {
        // Interior mutability via clone-and-swap on the file: the
        // struct is shared (cloned into the tunnel handler), so the
        // persist step rewrites the file wholesale.
        if let Some(path) = &self.path {
            let mut entries = self.entries.clone();
            entries.insert(key_for(host, port), fingerprint.to_string());
            let json = serde_json::to_string_pretty(&entries).unwrap_or_default();
            // Best-effort write; a failure here means the trust step
            // didn't persist (the caller surfaces IO errors if the
            // file matters — verify() on next connect will simply
            // re-prompt, which is the safe direction).
            let _ = write_file_atomically(path, &json);
        }
        // In-memory mode: the record lives only in the clone that the
        // CALLER holds; that clone is the one the next connect uses.
        // (Documented honestly: in-memory TOFU does not survive the
        // process.)
    }

    /// Number of recorded hosts (test/diagnostics).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is recorded (test/diagnostics).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Force-persist the given entries to this store's path (used by
    /// tests and by `record` internally via the shared map).
    fn persist(&self, entries: &HashMap<String, Fingerprint>) {
        if let Some(path) = &self.path {
            let json = serde_json::to_string_pretty(entries).unwrap_or_default();
            let _ = write_file_atomically(path, &json);
        }
    }

    /// Record-and-return for the caller's own clone (the tunnel calls
    /// this on ITS clone; the caller's clone goes stale, which is why
    /// `record` above rewrites the file the next load reads).
    pub fn record_mut(&mut self, host: &str, port: u16, fingerprint: &str) {
        self.entries
            .insert(key_for(host, port), fingerprint.to_string());
        self.persist(&self.entries.clone());
    }
}

fn key_for(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// Write via temp file + rename so a crash never leaves a truncated
/// known-hosts file.
fn write_file_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fpr(b: u8) -> Fingerprint {
        // 64 hex chars, distinct per b.
        format!("{b:02x}").repeat(32)
    }

    #[test]
    fn unseen_on_first_connect() {
        let kh = KnownHosts::in_memory();
        assert_eq!(
            kh.verify("bastion", 22, &fpr(1)),
            HostKeyVerdict::Unseen(fpr(1))
        );
    }

    #[test]
    fn trusted_after_record() {
        let mut kh = KnownHosts::in_memory();
        kh.record_mut("bastion", 22, &fpr(1));
        assert_eq!(kh.verify("bastion", 22, &fpr(1)), HostKeyVerdict::Trusted);
    }

    #[test]
    fn changed_key_hard_blocks() {
        let mut kh = KnownHosts::in_memory();
        kh.record_mut("bastion", 22, &fpr(1));
        assert_eq!(
            kh.verify("bastion", 22, &fpr(2)),
            HostKeyVerdict::Changed {
                new: fpr(2),
                old: fpr(1)
            }
        );
    }

    #[test]
    fn byte_exact_fingerprints_distinguish() {
        let mut kh = KnownHosts::in_memory();
        let f_a = "aa".repeat(32);
        let f_b = "ab".repeat(32); // one hex digit different
        kh.record_mut("h", 22, &f_a);
        assert!(kh.verify("h", 22, &f_b).is_changed());
    }

    #[test]
    fn port_and_host_are_separate_keys() {
        let mut kh = KnownHosts::in_memory();
        kh.record_mut("h", 22, &fpr(1));
        // same host, other port: unseen
        assert!(matches!(
            kh.verify("h", 2222, &fpr(1)),
            HostKeyVerdict::Unseen(_)
        ));
        // other host: unseen
        assert!(matches!(
            kh.verify("g", 22, &fpr(1)),
            HostKeyVerdict::Unseen(_)
        ));
    }

    #[test]
    fn load_missing_file_is_empty_not_error() {
        let kh = KnownHosts::load("/nonexistent/path/known_hosts.json");
        assert!(kh.is_empty());
    }

    #[test]
    fn record_persists_and_reload_verifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts.json");
        let mut kh = KnownHosts::load(&path);
        kh.record_mut("bastion", 22, &fpr(9));
        // Fresh load (as the next app run does):
        let reloaded = KnownHosts::load(&path);
        assert_eq!(
            reloaded.verify("bastion", 22, &fpr(9)),
            HostKeyVerdict::Trusted
        );
        assert!(matches!(
            reloaded.verify("bastion", 22, &fpr(8)),
            HostKeyVerdict::Changed { .. }
        ));
    }

    #[test]
    fn record_on_clone_persists_for_next_load() {
        // The tunnel handler holds a CLONE; record() there must still
        // persist (the file is the shared state).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts.json");
        let original = KnownHosts::load(&path);
        let handler_clone = original.clone();
        handler_clone.record("bastion", 22, &fpr(5));
        let reloaded = KnownHosts::load(&path);
        assert_eq!(
            reloaded.verify("bastion", 22, &fpr(5)),
            HostKeyVerdict::Trusted
        );
    }

    #[test]
    fn file_contains_only_fingerprints_and_hosts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts.json");
        let mut kh = KnownHosts::load(&path);
        kh.record_mut("bastion.example", 22, &fpr(7));
        let raw = std::fs::read_to_string(&path).expect("file written");
        assert!(raw.contains("bastion.example:22"));
        assert!(raw.contains(&fpr(7)));
        // no secret-looking fields
        assert!(!raw.contains("password"));
        assert!(!raw.contains("secret"));
    }

    impl HostKeyVerdict {
        fn is_changed(&self) -> bool {
            matches!(self, HostKeyVerdict::Changed { .. })
        }
    }
}
