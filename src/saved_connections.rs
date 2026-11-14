//! The saved-connections sidebar (Phase 6, spec item 4): a left-rail
//! list of `SavedConnection` entries, the save-current-form dialog, the
//! forget confirm modal, and the prefill flow.
//!
//! **This module renders and owns input entities; all persistence goes
//! through `connections-store`** (never a hand-rolled file write). The
//! workspace composes it into the left panel. Config dir resolution
//! (env override `REDIS_GPUI_CONFIG_DIR`, then
//! `~/.config/redis-gpui`) is connections-store's `config_dir()`.
//!
//! Privacy (PRD): the list shows name + host:port + TLS/SSH mode chips
//! only. Passwords are never displayed, never logged; the prefill path
//! loads them via `CredentialStore` and either fills the password
//! field or degrades to the PromptPerSession note (acceptance 7).

use std::sync::Arc;

use connections_store::credentials::{CredentialStore, KeyringStore};
use connections_store::saved_connections::{
    config_dir, load_connection, save_connection, SavedConnection,
};
use gpui::{div, prelude::*};

use crate::security::SecurityFormSnapshot;
use crate::text_input::SingleLineInput;

/// The core connect-form values for a save (everything except the
/// Security section, which travels in []).
pub struct ConnectionFormValues {
    /// User-chosen display name.
    pub name: String,
    /// Redis host.
    pub host: String,
    /// Redis port.
    pub port: u16,
    /// Logical database index.
    pub db: i64,
    /// ACL username (optional).
    pub username: Option<String>,
}

/// The saved-connections UI state the workspace owns.
pub struct SavedConnectionsPanel {
    /// The entries (a snapshot of `<config_dir>/connections/*.json`).
    entries: Vec<SavedConnection>,
    /// The save-dialog name input.
    save_name_input: gpui::Entity<SingleLineInput>,
    /// The save dialog's "save password in keychain" checkbox state.
    save_password: bool,
    /// The save dialog is open.
    save_dialog_open: bool,
    /// The forget-confirm modal: which connection id.
    forget_pending: Option<String>,
    /// The prefill note (PromptPerSession UX) shown under the form.
    prefill_note: Option<&'static str>,
    /// The credential store (the real OS keyring; tests swap the mock).
    store: Arc<dyn CredentialStore>,
}

impl SavedConnectionsPanel {
    /// Create the panel and load saved entries from the config dir.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            entries: Vec::new(),
            save_name_input: cx.new(|cx| SingleLineInput::new("name", "", cx)),
            save_password: true,
            save_dialog_open: false,
            forget_pending: None,
            prefill_note: None,
            store: Arc::new(KeyringStore),
        };
        this.reload();
        this
    }

    /// Re-read the connections dir (non-secret metadata only).
    fn reload(&mut self) {
        self.entries = list_saved();
    }

    /// The entries snapshot (for rendering).
    pub fn entries(&self) -> &[SavedConnection] {
        &self.entries
    }

    /// The current prefill note, if any.
    pub fn prefill_note(&self) -> Option<&'static str> {
        self.prefill_note
    }

    /// The save dialog's open state.
    pub fn save_dialog_open(&self) -> bool {
        self.save_dialog_open
    }

    /// The save dialog's name input entity (workspace subscribes for
    /// keystrokes; the input is plain text, non-secret).
    pub fn save_name_input(&self) -> &gpui::Entity<SingleLineInput> {
        &self.save_name_input
    }

    /// Whether the save dialog will store the password in the keychain.
    pub fn save_password(&self) -> bool {
        self.save_password
    }

    /// Toggle "save password in keychain" (default on).
    pub fn toggle_save_password(&mut self, cx: &mut Context<Self>) {
        self.save_password = !self.save_password;
        cx.notify();
    }

    /// Open the save-current-form dialog with the form's suggested name
    /// (`host:port`).
    pub fn open_save_dialog(&mut self, suggested: String, cx: &mut Context<Self>) {
        self.save_dialog_open = true;
        self.save_password = true;
        self.prefill_note = None;
        let input = self.save_name_input.clone();
        let ws_id = input.entity_id();
        let suggested = suggested.clone();
        let _ = cx.with_window(ws_id, |window: &mut gpui::Window, cx| {
            input.update(cx, |field, cx| {
                field.set_content(&suggested, window, cx);
            });
        });
        cx.notify();
    }

    /// Close the save dialog without saving.
    pub fn cancel_save_dialog(&mut self, cx: &mut Context<Self>) {
        self.save_dialog_open = false;
        cx.notify();
    }

    /// Save the current form. Returns the outcome as a user note:
    /// `Ok(note)` surfaces "saved" (or the PromptPerSession degrade —
    /// nothing was written then); `Err(msg)` is a display error.
    ///
    /// `secret` is passed only when the checkbox is on; the store layer
    /// writes it to the keyring FIRST (nothing hits disk if the store
    /// is unavailable).
    pub fn save_current(
        &mut self,
        values: &ConnectionFormValues,
        snapshot: &SecurityFormSnapshot,
        password: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<Option<&'static str>, String> {
        let ConnectionFormValues {
            name,
            host,
            port,
            db,
            username,
        } = values;
        let id = crate::security::slug_for(&format!("{name}-{host}-{port}"));
        let conn = SavedConnection {
            id: id.clone(),
            name: name.trim().to_string(),
            host: host.trim().to_string(),
            port: *port,
            db: *db,
            username: username.clone(),
            tls: snapshot.tls_policy(),
            ssh: snapshot.ssh_policy(),
        };
        let outcome = save_connection(&conn, password.as_deref(), self.store.as_ref())?;
        match outcome {
            connections_store::saved_connections::SaveOutcome::Saved => {
                self.save_dialog_open = false;
                self.reload();
                self.prefill_note = None;
                cx.notify();
                Ok(None)
            }
            connections_store::saved_connections::SaveOutcome::PromptPerSession => {
                // Keyring unavailable: NOTHING was written; the user
                // re-enters the password each session (acceptance 7).
                self.prefill_note = Some(crate::security::NOTE_KEYCHAIN_UNAVAILABLE);
                cx.notify();
                Ok(self.prefill_note)
            }
        }
    }

    /// Prefill payload for clicking a saved entry: the connection
    /// metadata + the credential outcome for the password field.
    /// `Err` carries a display error (never the secret).
    pub fn prefill(
        &mut self,
        conn_id: &str,
    ) -> Result<
        (
            SavedConnection,
            connections_store::credentials::CredentialOutcome<String>,
        ),
        String,
    > {
        let conn = load_connection(conn_id)?.ok_or("saved connection not found")?;
        let secret =
            connections_store::saved_connections::load_secret(conn_id, self.store.as_ref())?;
        self.prefill_note = crate::security::prefill_plan(&secret).note;
        Ok((conn, secret))
    }

    /// Open the forget-confirm modal for this connection.
    pub fn request_forget(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        self.forget_pending = Some(conn_id.to_string());
        cx.notify();
    }

    /// The pending forget confirm (the connection id), if any.
    pub fn forget_pending(&self) -> Option<&str> {
        self.forget_pending.as_deref()
    }

    /// Cancel the forget confirm.
    pub fn cancel_forget(&mut self, cx: &mut Context<Self>) {
        self.forget_pending = None;
        cx.notify();
    }

    /// Confirm forget: delete the metadata file AND the keychain entry
    /// (via connections-store; never hand-rolled).
    pub fn confirm_forget(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(id) = self.forget_pending.take() else {
            return Ok(());
        };
        connections_store::saved_connections::delete_connection(&id, self.store.as_ref())?;
        self.reload();
        cx.notify();
        Ok(())
    }
}

impl gpui::EventEmitter<()> for SavedConnectionsPanel {}

impl gpui::Render for SavedConnectionsPanel {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Pass-through container: the workspace composes the real content.
        div().size_full()
    }
}

/// List saved connections from the config dir (metadata only; missing
/// dir → empty). Best-effort: a malformed file is skipped (a corrupt
/// file must not take the sidebar down).
pub fn list_saved() -> Vec<SavedConnection> {
    let dir = config_dir().join("connections");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(Some(conn)) = load_connection(id) {
            out.push(conn);
        }
    }
    out.sort_by_key(|c| c.name.to_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use connections_store::credentials::MockCredentialStore;

    /// Serialize env-var scopes across tests (the process-global trap).
    /// This re-exports settings' lock so every module mutating
    /// `REDIS_GPUI_CONFIG_DIR` shares ONE mutex (a per-module lock still
    /// races under the multi-threaded runner — the settings tests take
    /// the same guard).
    fn with_config_dir(f: impl FnOnce(&std::path::Path)) {
        let _guard = crate::settings::SETTINGS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("REDIS_GPUI_CONFIG_DIR", dir.path());
        f(dir.path());
        std::env::remove_var("REDIS_GPUI_CONFIG_DIR");
    }

    #[test]
    fn list_saved_is_empty_for_missing_dir() {
        with_config_dir(|dir| {
            assert!(list_saved().is_empty());
            assert!(!dir.join("connections").exists());
        });
    }

    #[test]
    fn reload_after_save_lists_the_entry_sorted() {
        with_config_dir(|_| {
            let store = MockCredentialStore::working();
            let snap = SecurityFormSnapshot::default();
            let conn = SavedConnection {
                id: "b".into(),
                name: "Beta".into(),
                host: "h".into(),
                port: 6379,
                db: 0,
                username: None,
                tls: snap.tls_policy(),
                ssh: snap.ssh_policy(),
            };
            save_connection(&conn, None, &store).expect("save");
            let conn2 = SavedConnection {
                id: "a".into(),
                name: "alpha".into(),
                ..conn.clone()
            };
            save_connection(&conn2, None, &store).expect("save");
            let listed = list_saved();
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[0].name, "alpha");
            assert_eq!(listed[1].name, "Beta");
        });
    }

    #[test]
    fn malformed_files_are_skipped_not_fatal() {
        with_config_dir(|dir| {
            let conns = dir.join("connections");
            std::fs::create_dir_all(&conns).unwrap();
            std::fs::write(conns.join("good.json"), r#"{"id":"good","name":"g","host":"h","port":6379,"db":0,"username":null,"tls":{"mode":"Disabled"},"ssh":null}"#).unwrap();
            std::fs::write(conns.join("bad.json"), "not json").unwrap();
            let listed = list_saved();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].name, "g");
        });
    }

    #[test]
    fn slug_ids_are_file_safe_and_stable() {
        assert_eq!(
            crate::security::slug_for("prod-redis-internal-6379"),
            "prod-redis-internal-6379"
        );
    }
}
