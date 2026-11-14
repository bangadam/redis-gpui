//! Acceptance-3 fault-injection workflow test battery (Phase 5, test-first).
//!
//! PRD acceptance 3, stories 26-29 + 31; PLAN.md §5/§7 boundary tests
//! 3 and 9, plus the gate stories. All headless: no GPUI, no server —
//! the `ScriptedConnection` mock + redis-io's feature-gated
//! `FaultInjectingConnection` produce the nasty network cases
//! deterministically.

use app_workflows::mock::{MockStep, ScriptedConnection};
use app_workflows::save_workflow::{
    Reconnectable, SaveFlow, SaveFlowError, SaveRefusal, SaveWorkflow,
};
use redis_io::connection::Command;
use redis_io::faults::{FaultInjectingConnection, FaultKind, FaultSeam};
use value_model::editor::{EditedValue, Slot};
use value_model::raw::{RawBytes, RawValue};
use write_safety::gate::GateState;

fn string_value(bytes: &[u8]) -> RawValue {
    RawValue::String(RawBytes::new(bytes.to_vec()))
}

fn hash_value() -> RawValue {
    RawValue::Hash(vec![
        (RawBytes::from("field1"), RawBytes::from("value1")),
        (RawBytes::from("field2"), RawBytes::from(vec![0xC3, 0x28])),
    ])
}

// ---------------------------------------------------------------------
// 1. Happy path
// ---------------------------------------------------------------------

/// AC-basic (story 28): explicit Save with a clean ack → editor clean,
/// not dirty, not diverged.
#[tokio::test]
async fn save_ack_happy_path() {
    let (conn, log) =
        ScriptedConnection::new(vec![MockStep::Reply(redis_io::connection::Reply::Okay)]);
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);

    wf.enable_editing();
    assert_eq!(wf.gate_state(), GateState::WriteEnabled);
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "edited");
    assert!(wf.editor().dirty);

    let flow = wf.save().await;
    assert!(matches!(flow, SaveFlow::Acknowledged));

    // Editor clean: not dirty, not diverged, original = the edit.
    let editor = wf.editor();
    assert!(!editor.dirty);
    assert!(!editor.diverged_from_server);
    assert_eq!(editor.original, string_value(b"edited"));

    // Exactly one SET was issued, with the edited bytes.
    assert_eq!(log.issue_count(), 1);
    assert_eq!(
        log.issued_at(0).unwrap(),
        Command::new("SET", vec![b"mykey".to_vec(), b"edited".to_vec()])
    );
}

// ---------------------------------------------------------------------
// 2. THE acceptance-3 test
// ---------------------------------------------------------------------

/// PRD acceptance 3 / PLAN §7 boundary test 3: a dropped write
/// acknowledgment after a disconnect reports OutcomeUnknown, preserves
/// the editor's original, and NEVER re-sends — proven by call count.
#[tokio::test]
async fn dropped_ack_reports_outcome_unknown_and_never_resends() {
    // Fault seam (a): the SET is really issued exactly once, its reply
    // is swallowed, the link severs, and the wrapper stays sticky-broken.
    let (inner, log) = ScriptedConnection::new(vec![]);
    let conn: FaultInjectingConnection<ScriptedConnection> = FaultInjectingConnection::new(
        inner,
        vec![FaultSeam::on(
            FaultKind::DropReplyThenDisconnect,
            vec!["SET".into()],
        )],
    );

    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);
    wf.enable_editing();
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");

    let original_before = wf.editor().original.clone();

    let flow = wf.save().await;

    // (a) OutcomeUnknown, with the honest check-before-retrying state.
    assert!(
        flow.is_outcome_unknown(),
        "expected OutcomeUnknown, got {flow:?}"
    );
    if let SaveFlow::OutcomeUnknown {
        check_before_retrying,
    } = &flow
    {
        assert!(
            check_before_retrying.contains("check"),
            "honest wording: {check_before_retrying}"
        );
    }

    // (b) The exact SET was issued EXACTLY ONCE.
    assert_eq!(log.issue_count(), 1, "the SET must be issued exactly once");
    assert_eq!(
        log.issued_at(0).unwrap(),
        Command::new("SET", vec![b"mykey".to_vec(), b"my edit".to_vec()])
    );

    // (c) The editor is UNTOUCHED: still dirty, original preserved
    //     byte-exact (a later re-read can compare), NOT acknowledged.
    let editor = wf.editor();
    assert!(editor.dirty, "editor must stay dirty under OutcomeUnknown");
    assert!(!editor.diverged_from_server);
    assert_eq!(
        editor.original, original_before,
        "original bytes must be preserved for a later re-read"
    );
    match &editor.working {
        EditedValue::Scalar(Slot::Edited(bytes)) => {
            assert_eq!(bytes.as_bytes(), b"my edit");
        }
        other => panic!("working buffer clobbered: {other:?}"),
    }

    // (d) The gate reset to read-only at the disconnect boundary.
    assert_eq!(wf.gate_state(), GateState::ReadOnly);

    // (e) A retry attempt through the severed connection sends NOTHING
    //     (sticky-broken): zero NEW writes were issued.
    let issued_before = log.issue_count();
    // The user (deliberately) re-enables editing and retries; the
    // sticky-broken connection refuses to send anything.
    wf.enable_editing();
    let retry = wf.save().await;
    assert_eq!(
        log.issue_count(),
        issued_before,
        "no resend through the severed link"
    );
    assert!(
        retry.is_outcome_unknown(),
        "sticky-broken stays OutcomeUnknown"
    );
}

// ---------------------------------------------------------------------
// 3. Definitive failure
// ---------------------------------------------------------------------

/// Story 28: a server error reply (WRONGTYPE-style) → Failed, editor
/// unchanged and dirty, retry allowed (user action, not auto).
#[tokio::test]
async fn definitive_failure_leaves_editor_dirty_recoverable() {
    let (conn, log) = ScriptedConnection::new(vec![
        MockStep::Definitive("WRONGTYPE".into()),
        MockStep::Definitive("WRONGTYPE".into()),
    ]);
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);

    wf.enable_editing();
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");

    let flow = wf.save().await;
    match &flow {
        SaveFlow::Failed(SaveFlowError::Definitive(msg)) => {
            assert!(msg.contains("WRONGTYPE"))
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // Editor untouched: dirty, original intact.
    let editor = wf.editor();
    assert!(editor.dirty);
    assert_eq!(editor.original, string_value(b"original"));

    // The gate did NOT reset (the failure was definitive, not a disconnect).
    assert_eq!(wf.gate_state(), GateState::WriteEnabled);

    // Retry is allowed as an explicit user action (new command issued).
    let issued_before = log.issue_count();
    let retry = wf.save().await;
    assert_eq!(
        log.issue_count(),
        issued_before + 1,
        "user-driven retry issues again"
    );
    assert!(matches!(retry, SaveFlow::Failed(_)));
}

// ---------------------------------------------------------------------
// 4. Gate refusals (stories 26-27)
// ---------------------------------------------------------------------

/// Story 26: read-only gate → the save refuses BEFORE any issue (call
/// count zero), with the write-safety refusal wording.
#[tokio::test]
async fn gate_blocks_save_when_readonly() {
    let (conn, log) = ScriptedConnection::new(vec![]);
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);

    // NO enable_editing: the gate stays read-only.
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");
    assert!(wf.editor().dirty);

    let flow = wf.save().await;
    match &flow {
        SaveFlow::Refused(SaveRefusal::GateReadOnly) => {}
        other => panic!("expected GateReadOnly refusal, got {other:?}"),
    }

    // Refusal wording is the VERBATIM write-safety contract.
    assert_eq!(
        SaveRefusal::GateReadOnly.message(),
        write_safety::BlockedReason::WriteBlockedByReadOnly.message()
    );

    // Zero commands issued: the refusal is client-side, pre-send.
    assert_eq!(log.issue_count(), 0, "read-only refusal must send nothing");
    // Editor untouched: still dirty, original intact.
    assert!(wf.editor().dirty);
}

/// Story 27: WriteEnabled + Disconnected event → gate ReadOnly; a save
/// attempt then refuses with zero issues (the reset-on-disconnect rule).
#[tokio::test]
async fn gate_resets_on_disconnect() {
    let (conn, log) = ScriptedConnection::new(vec![]);
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);

    wf.enable_editing();
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");

    // The disconnect event arrives (e.g. the event channel surfaces it).
    wf.on_disconnected();
    assert_eq!(wf.gate_state(), GateState::ReadOnly);

    let flow = wf.save().await;
    assert!(matches!(flow, SaveFlow::Refused(SaveRefusal::GateReadOnly)));
    assert_eq!(
        log.issue_count(),
        0,
        "post-disconnect save must send nothing"
    );
    // The editor's work survives the gate reset (dirty, original intact).
    assert!(wf.editor().dirty);
}

// ---------------------------------------------------------------------
// 5. Refresh survival (acceptance 9)
// ---------------------------------------------------------------------

/// PRD acceptance 9: refresh mid-dirty → original updated, diverged=true,
/// working untouched — byte-exact.
#[tokio::test]
async fn dirty_editor_survives_refresh() {
    let (conn, _log) = ScriptedConnection::new(vec![]);
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"v1"), conn);

    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");
    assert!(wf.editor().dirty);

    // Refresh: the server's value changed underneath.
    wf.apply_refresh(string_value(b"server version"));

    let editor = wf.editor();
    assert!(editor.diverged_from_server, "divergence flag must be set");
    assert_eq!(editor.original, string_value(b"server version"));
    match &editor.working {
        EditedValue::Scalar(Slot::Edited(bytes)) => {
            assert_eq!(bytes.as_bytes(), b"my edit", "working buffer byte-exact");
        }
        other => panic!("working was clobbered: {other:?}"),
    }
    assert!(editor.dirty, "still dirty after refresh");

    // The CAS precheck detects a FURTHER server change: the refresh
    // updated the original to "server version", but the server has since
    // moved on again — the GET returns yet newer bytes, which differ from
    // the (refreshed) original → conflict.
    let (conn2, _log2) = ScriptedConnection::new(vec![MockStep::Reply(
        redis_io::connection::Reply::Bulk(b"even newer than the refresh".to_vec()),
    )]);
    let mut wf2 = wf.replace_connection(conn2);
    let precheck = wf2.conflict_precheck().await;
    assert!(matches!(precheck, Err(SaveRefusal::Conflict)));
}

// ---------------------------------------------------------------------
// 6. Conflict precheck (PLAN F5, PRD acceptance 9)
// ---------------------------------------------------------------------

/// PLAN.md F5: strings — if the original changed server-side, the save
/// refuses with a conflict (never overwrites silently); if unchanged, it
/// proceeds. Two mock setups.
#[tokio::test]
async fn conflict_precheck_cas_supported() {
    // --- Setup A: server value CHANGED under the edit → refuse. ---
    let (conn, log) = ScriptedConnection::new(vec![MockStep::Reply(
        redis_io::connection::Reply::Bulk(b"someone else won".to_vec()),
    )]);
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);
    wf.enable_editing();
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");

    let flow = wf.save_with_conflict_precheck().await;
    match &flow {
        SaveFlow::Refused(SaveRefusal::Conflict) => {}
        other => panic!("expected Conflict refusal, got {other:?}"),
    }
    // Only the GET was issued — the SET never left (no silent overwrite).
    assert_eq!(log.issue_count(), 1);
    assert_eq!(log.issued_at(0).unwrap().name, "GET");
    assert!(wf.editor().dirty, "editor kept the user's work");

    // --- Setup B: server value UNCHANGED → proceeds to the save. ---
    let (conn, log) = ScriptedConnection::new(vec![
        MockStep::Reply(redis_io::connection::Reply::Bulk(b"original".to_vec())),
        MockStep::Reply(redis_io::connection::Reply::Okay),
    ]);
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);
    wf.enable_editing();
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");

    let flow = wf.save_with_conflict_precheck().await;
    assert!(matches!(flow, SaveFlow::Acknowledged), "got {flow:?}");
    // GET then SET, in that order.
    assert_eq!(log.issue_count(), 2);
    assert_eq!(log.issued_at(0).unwrap().name, "GET");
    assert_eq!(log.issued_at(1).unwrap().name, "SET");
    assert!(!wf.editor().dirty);
}

// ---------------------------------------------------------------------
// 7. Hash save (the edited-field-only encoder)
// ---------------------------------------------------------------------

/// Story 31: editing one hash field issues one HSET for that field only;
/// the untouched invalid-UTF-8 sibling is never re-encoded (not even sent).
#[tokio::test]
async fn hash_edit_saves_only_the_edited_field() {
    let (conn, log) =
        ScriptedConnection::new(vec![MockStep::Reply(redis_io::connection::Reply::Okay)]);
    let mut wf = SaveWorkflow::open(b"h".to_vec(), hash_value(), conn);
    wf.enable_editing();
    wf.edit_text(&value_model::editor::FieldPath::HashField(0), "new value1");

    let flow = wf.save().await;
    assert!(matches!(flow, SaveFlow::Acknowledged));
    assert!(!wf.editor().dirty);

    // One HSET for the edited field only.
    assert_eq!(log.issue_count(), 1);
    assert_eq!(
        log.issued_at(0).unwrap(),
        Command::new(
            "HSET",
            vec![b"h".to_vec(), b"field1".to_vec(), b"new value1".to_vec()]
        )
    );
}

// ---------------------------------------------------------------------
// 8. Reconnect proof (the sticky-broken contract, by call count)
// ---------------------------------------------------------------------

/// The reconnect-issues-zero-writes proof: after OutcomeUnknown, an
/// explicit reconnect (fresh connection) sends zero writes; a subsequent
/// read-only check (GET) is the only traffic.
#[tokio::test]
async fn reconnect_after_outcome_unknown_sends_no_writes() {
    let (inner, log) = ScriptedConnection::new(vec![]);
    let conn: FaultInjectingConnection<ScriptedConnection> = FaultInjectingConnection::new(
        inner,
        vec![FaultSeam::on(
            FaultKind::DropReplyThenDisconnect,
            vec!["SET".into()],
        )],
    );
    let mut wf = SaveWorkflow::open(b"mykey".to_vec(), string_value(b"original"), conn);
    wf.enable_editing();
    wf.edit_text(&value_model::editor::FieldPath::Scalar, "my edit");

    let flow = wf.save().await;
    assert!(flow.is_outcome_unknown());
    assert_eq!(log.issue_count(), 1, "SET issued exactly once");

    // Reconnect (explicit, user-initiated): the workflow swaps in a fresh
    // connection (the mirror of RedisConnection::reconnect — re-sends
    // nothing; the severed socket is simply gone).
    let (fresh, fresh_log) = ScriptedConnection::new(vec![MockStep::Reply(
        redis_io::connection::Reply::Bulk(b"my edit".to_vec()),
    )]);
    let mut wf2 = wf.replace_connection(fresh);

    // Zero writes issued by the fresh connection so far.
    assert_eq!(fresh_log.issue_count(), 0);

    // The honest check: a read-only GET (the user checked; the read
    // cannot PROVE the write's outcome, but it is the user's deliberate
    // action — never an automatic resend).
    let current = wf2.read_current().await;
    assert_eq!(current, Some(b"my edit".to_vec()));
    // Still zero WRITES: the only traffic is the GET.
    assert_eq!(fresh_log.issue_count(), 1);
    assert_eq!(fresh_log.issued_at(0).unwrap().name, "GET");
    assert!(
        fresh_log.count_of("SET") == 0,
        "no write was re-sent after reconnect"
    );

    // And the OLD connection's log still shows exactly one SET.
    assert_eq!(log.count_of("SET"), 1);

    // The mock's own Reconnectable impl: reconnect sends nothing.
    let (direct, direct_log) = ScriptedConnection::new(vec![]);
    let mut rc = direct;
    <ScriptedConnection as Reconnectable>::reconnect(&mut rc)
        .await
        .unwrap();
    assert_eq!(direct_log.issue_count(), 0);
}
