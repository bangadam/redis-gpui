//! Mock connection + fault-injection tests (acceptance 3, PRD/PLAN §7).
//!
//! These run WITHOUT any Redis server: the mock is scriptable and counts
//! `issue()` calls so the "never re-sent" guarantee is checked directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use redis_io::connection::{Command, ConnError, Connection, Reply, ScanResult};
use redis_io::outcome::WriteOutcome;

/// Scriptable mock. Each `issue` pops the next scripted step.
/// Counts every `issue()` call so tests can prove a command was
/// NOT re-issued.
struct MockConnection {
    steps: Vec<MockStep>,
    issue_calls: Arc<AtomicU64>,
    disconnected: bool,
}

enum MockStep {
    Reply(Reply),
    Definitive(String),
    Disconnect,
}

impl MockConnection {
    fn new(steps: Vec<MockStep>) -> (Self, Arc<AtomicU64>) {
        let issue_calls = Arc::new(AtomicU64::new(0));
        (
            Self {
                steps,
                issue_calls: issue_calls.clone(),
                disconnected: false,
            },
            issue_calls,
        )
    }

    #[cfg_attr(not(feature = "fault-injection"), allow(dead_code))]
    fn issue_count(counter: &Arc<AtomicU64>) -> u64 {
        counter.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Connection for MockConnection {
    async fn issue(&mut self, _cmd: Command) -> Result<Reply, ConnError> {
        self.issue_calls.fetch_add(1, Ordering::SeqCst);
        match self.steps.pop() {
            Some(MockStep::Reply(r)) => Ok(r),
            Some(MockStep::Definitive(msg)) => Err(ConnError::Definitive(
                redis_io::outcome::DefinitiveError::new(
                    redis_io::outcome::DefinitiveErrorKind::ServerError,
                    msg,
                ),
            )),
            Some(MockStep::Disconnect) => {
                self.disconnected = true;
                Err(ConnError::Disconnected {
                    reason: Some("mock: server closed link".into()),
                    // The real backend always attaches the in-flight command;
                    // the mock models the post-boundary state (None).
                    in_flight: None,
                })
            }
            None => Err(ConnError::Disconnected {
                reason: Some("mock: exhausted".into()),
                in_flight: None,
            }),
        }
    }

    async fn scan_batch(
        &mut self,
        cursor: u64,
        _pattern: Option<Vec<u8>>,
        _count: u64,
    ) -> Result<ScanResult, ConnError> {
        if self.disconnected {
            return Err(ConnError::Disconnected {
                reason: Some("mock: severed".into()),
                in_flight: None,
            });
        }
        Ok(ScanResult {
            next_cursor: 0,
            keys: vec![vec![0, 159]],
        })
        .map(|_| {
            let _ = cursor;
            unreachable!()
        })
    }

    async fn ping(&mut self) -> Result<(), ConnError> {
        if self.disconnected {
            return Err(ConnError::Disconnected {
                reason: Some("mock: severed".into()),
                in_flight: None,
            });
        }
        Ok(())
    }
}

fn set_cmd() -> Command {
    Command::new("SET", vec![b"k".to_vec(), b"v".to_vec()])
}

// ---------- Acceptance 3: the fault-injection test ----------

#[cfg(feature = "fault-injection")]
mod fault_tests {
    use super::*;
    use redis_io::faults::{FaultInjectingConnection, FaultKind, FaultSeam};

    /// The deterministic acceptance-3 test:
    /// - `DropReplyThenDisconnect` swallows the reply for `SET` and severs.
    /// - The wrapper returns `ConnError::Disconnected` with `in_flight` set.
    /// - The caller maps it to `WriteOutcome::OutcomeUnknown`.
    /// - A naive "retry on error" loop would re-issue; we prove the command
    ///   is issued EXACTLY ONCE even when the caller retries, because the
    ///   wrapper is sticky-broken after the boundary (the honest model of
    ///   the real backend).
    #[tokio::test]
    async fn drop_reply_then_disconnect_yields_outcome_unknown_and_never_resends() {
        // Mock pops steps from the END of the list, so the script is
        // written in reverse: one Ok reply for the single real issue.
        let (mock, counter) = MockConnection::new(vec![MockStep::Reply(Reply::Okay)]);
        let seam = FaultSeam::on(FaultKind::DropReplyThenDisconnect, vec!["SET".into()]);
        let mut conn = FaultInjectingConnection::new(mock, vec![seam]);

        // Simulate the app's write path (what Phase 2 will do):
        let cmd = set_cmd();
        let result = conn.issue(cmd.clone()).await;

        // The wrapper surfaces Disconnected with the in-flight command:
        match &result {
            Err(ConnError::Disconnected { in_flight, .. }) => {
                assert_eq!(in_flight.as_ref(), Some(&cmd));
            }
            Ok(r) => panic!("expected Disconnected, got Ok({r:?})"),
            Err(e) => panic!("expected Disconnected, got {e:?}"),
        }

        // The call site maps it to the tri-state outcome:
        let outcome = match result {
            Err(e) => WriteOutcome::from_conn_error(cmd, e),
            Ok(_) => unreachable!(),
        };
        assert!(outcome.is_outcome_unknown());

        // The naive auto-retry loop (what we forbid, modeled to prove the
        // guard): issue again → sticky Disconnected, nothing re-sent.
        let second = conn.issue(set_cmd()).await;
        assert!(matches!(
            second,
            Err(ConnError::Disconnected {
                in_flight: None,
                ..
            })
        ));
        // EXACTLY one issue reached the inner connection (the original
        // send). The retry did NOT produce a second issue:
        assert_eq!(MockConnection::issue_count(&counter), 1);
    }

    /// Other commands pass through untouched while the fault is armed.
    #[tokio::test]
    async fn fault_only_triggers_on_matching_commands() {
        // Reverse order (pop semantics): first pop -> SET's Okay (swallowed
        // by the seam), second pop -> GET's "42".
        let (mock, counter) = MockConnection::new(vec![
            MockStep::Reply(Reply::Okay), // consumed by the SET trigger
            MockStep::Reply(Reply::Bulk(b"42".to_vec())), // consumed by GET
        ]);
        let seam = FaultSeam::on(FaultKind::DropReplyThenDisconnect, vec!["SET".into()]);
        let mut conn = FaultInjectingConnection::new(mock, vec![seam]);

        let get = Command::new("GET", vec![b"k".to_vec()]);
        let r = conn.issue(get).await.expect("GET passes through");
        assert_eq!(r.as_bytes(), Some(b"42".as_slice()));

        let _ = conn.issue(set_cmd()).await; // triggers the fault

        assert_eq!(MockConnection::issue_count(&counter), 2);
    }
}

// ---------- Outcome mapping (no fault feature needed) ----------

#[tokio::test]
async fn mock_disconnect_without_in_flight_maps_callers_cmd_to_unknown() {
    let (mut mock, _counter) = MockConnection::new(vec![MockStep::Disconnect]);
    let cmd = set_cmd();
    let err = mock.issue(cmd.clone()).await.unwrap_err();
    let outcome = WriteOutcome::from_conn_error(cmd.clone(), err);
    match outcome {
        WriteOutcome::OutcomeUnknown(ctx) => assert_eq!(ctx.in_flight, cmd),
        _ => panic!("expected OutcomeUnknown"),
    }
}

#[tokio::test]
async fn mock_ack_and_definitive_map_to_ack_and_failed() {
    let (mut mock, _) = MockConnection::new(vec![MockStep::Reply(Reply::Okay)]);
    let reply = mock.issue(set_cmd()).await.expect("ok");
    assert!(matches!(reply, Reply::Okay));

    let (mut mock, _) = MockConnection::new(vec![MockStep::Definitive("WRONGTYPE".into())]);
    let err = mock.issue(set_cmd()).await.unwrap_err();
    let outcome = WriteOutcome::from_conn_error(set_cmd(), err);
    assert!(matches!(outcome, WriteOutcome::Failed(_)));
}
