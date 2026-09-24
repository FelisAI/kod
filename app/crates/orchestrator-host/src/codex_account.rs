//! Codex limits read from the ACCOUNT, not from a session (docs/019 codex limit).
//!
//! A codex limit belongs to the account, but a rollout's `token_count` telemetry
//! is written only when that session runs a turn. Read per session, the same
//! weekly window showed 58% on one idle session and 98% on a busy one at the same
//! moment (measured 2026-09-23), and a workspace running out of credits never
//! reached a session that wasn't mid-turn at all.
//!
//! `codex app-server` answers `account/rateLimits/read` for the whole account —
//! the backend's own verdict (`ordinaryUsageAllowed`), every limit bucket, and why
//! a block happened. It costs one short-lived app-server per read (~0.6s
//! measured, no model quota). The host reads it per account and applies it to
//! every session on that account; a session's rollout growing is only the
//! doorbell that says a turn just ran and the numbers moved.
//!
//! Its `account/rateLimits/updated` push is no substitute: codex sends it only
//! for turns run inside that same app-server (`bespoke_event_handling.rs`,
//! 0.155.1), and Kod's sessions are terminal processes of their own.
//!
//! Everything but [`read_account_limits`] is pure, so `cargo test` replays
//! recorded responses and never runs codex (RULE ZERO).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One window of a limit bucket (codex's team plans report only the weekly one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    pub used_percent: f64,
    pub window_mins: Option<u32>,
    /// unix seconds.
    pub resets_at: Option<i64>,
}

/// The account's ORDINARY codex usage, as the backend reports it in one read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountLimits {
    /// wall-clock ms the read came back.
    pub observed_ms: u64,
    /// `ordinaryUsageAllowed`: the backend's own "can this account run". `None`
    /// = unavailable, which the backend says must not be read as recovery.
    pub usage_allowed: Option<bool>,
    /// `rateLimitReachedType` of the ordinary (`codex`) bucket, e.g.
    /// `workspace_owner_credits_depleted`.
    pub reached: Option<String>,
    /// the ordinary bucket's windows. Other buckets (`premium`) are not read:
    /// they meter extras, and a depleted one leaves ordinary usage running.
    pub windows: Vec<LimitWindow>,
}

impl AccountLimits {
    /// Is the account blocked? The backend's `ordinaryUsageAllowed` decides when
    /// it answers. When it is null the backend does not know, and its schema says
    /// clients "must not infer recovery from percentages or reset times": a reached
    /// reason or a full window still reads as blocked, but nothing reads as
    /// recovered — `was_blocked` stands.
    pub fn is_blocked(&self, was_blocked: bool) -> bool {
        match self.usage_allowed {
            Some(allowed) => !allowed,
            None => {
                self.reached.is_some()
                    || self.windows.iter().any(|w| w.used_percent >= 100.0)
                    || was_blocked
            }
        }
    }
}

/// Lift the ordinary bucket out of an `account/rateLimits/read` result:
/// `rateLimitsByLimitId.codex`, else the single-bucket `rateLimits` view (which
/// mirrors it). `None` when the result says nothing usable.
pub fn parse_account_limits(result: &Value, observed_ms: u64) -> Option<AccountLimits> {
    let usage_allowed = result.get("ordinaryUsageAllowed").and_then(Value::as_bool);
    let bucket = result
        .get("rateLimitsByLimitId")
        .and_then(|m| m.get("codex"))
        .filter(|v| !v.is_null())
        .or_else(|| result.get("rateLimits").filter(|v| !v.is_null()));
    let reached = bucket
        .and_then(|b| b.get("rateLimitReachedType"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let windows: Vec<LimitWindow> = bucket
        .map(|b| {
            ["primary", "secondary"]
                .iter()
                .filter_map(|k| {
                    let w = b.get(*k)?;
                    Some(LimitWindow {
                        used_percent: w.get("usedPercent").and_then(Value::as_f64)?,
                        window_mins: w
                            .get("windowDurationMins")
                            .and_then(Value::as_u64)
                            .map(|m| m as u32),
                        resets_at: w.get("resetsAt").and_then(Value::as_i64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if usage_allowed.is_none() && reached.is_none() && windows.is_empty() {
        return None;
    }
    Some(AccountLimits {
        observed_ms,
        usage_allowed,
        reached,
        windows,
    })
}

/// Why a read produced nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadError {
    /// this codex has no `account/rateLimits/read` — stop asking; the session
    /// rollouts stay the source for this account.
    Unsupported(String),
    /// spawn failure, timeout, backend or auth error — keep what we had, retry
    /// on the next due read.
    Failed(String),
}

/// JSON-RPC "method not found" — the one error that means "this codex can't".
const METHOD_NOT_FOUND: i64 = -32601;

/// Ask a short-lived `<program> app-server`, run against `codex_home`, for the
/// account's rate limits: `initialize`, `initialized`, `account/rateLimits/read`,
/// then close its stdin (it exits on EOF; a kill is only the fallback). Blocks up
/// to `timeout` — call it OFF the sweep thread.
pub fn read_account_limits(
    program: &str,
    codex_home: &Path,
    timeout: Duration,
) -> Result<Value, ReadError> {
    let mut child = Command::new(program)
        .arg("app-server")
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ReadError::Failed(format!("could not start {program} app-server: {e}")))?;
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    if let Some(out) = stdout {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let send = |stdin: &mut Option<std::process::ChildStdin>, msg: Value| -> Result<(), ReadError> {
        let w = stdin
            .as_mut()
            .ok_or_else(|| ReadError::Failed("app-server stdin closed".into()))?;
        writeln!(w, "{msg}")
            .and_then(|_| w.flush())
            .map_err(|e| ReadError::Failed(format!("app-server write failed: {e}")))
    };
    let outcome = (|| {
        send(
            &mut stdin,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "clientInfo": {"name": "kod", "title": "Kod", "version": env!("CARGO_PKG_VERSION")}
            }}),
        )?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = rx.recv_timeout(left).map_err(|e| {
                ReadError::Failed(match e {
                    mpsc::RecvTimeoutError::Timeout => "app-server did not answer in time".into(),
                    mpsc::RecvTimeoutError::Disconnected => "app-server exited without answering".into(),
                })
            })?;
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(err) = msg.get("error") {
                let code = err.get("code").and_then(Value::as_i64);
                let text = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("app-server error")
                    .to_string();
                return Err(if code == Some(METHOD_NOT_FOUND) {
                    ReadError::Unsupported(text)
                } else {
                    ReadError::Failed(text)
                });
            }
            match msg.get("id").and_then(Value::as_u64) {
                Some(1) => {
                    send(&mut stdin, json!({"jsonrpc": "2.0", "method": "initialized"}))?;
                    send(
                        &mut stdin,
                        json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read", "params": null}),
                    )?;
                }
                Some(2) => {
                    return msg
                        .get("result")
                        .cloned()
                        .ok_or_else(|| ReadError::Failed("app-server reply had no result".into()));
                }
                // notifications and anything else codex volunteers
                _ => {}
            }
        }
    })();
    // EOF is how an app-server is told to stop; the kill only covers one that won't.
    drop(stdin);
    let stop_by = Instant::now() + Duration::from_secs(2);
    while Instant::now() < stop_by {
        if let Ok(Some(_)) = child.try_wait() {
            return outcome;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

/// Fastest re-read after the doorbell rings — a busy account reads at most this
/// often however fast its rollouts grow.
pub const READ_MIN_GAP_MS: u64 = 10_000;
/// While the account is blocked: read often enough that the moment it can run
/// again (a reset, credits added) reaches the ⛔ row and auto-continue promptly.
pub const READ_BLOCKED_EVERY_MS: u64 = 60_000;
/// Otherwise: catch what no local rollout will ever show — another device, the
/// workspace owner's credits, the weekly reset of an idle account.
pub const READ_IDLE_EVERY_MS: u64 = 300_000;

/// Is a read due for an account? `doorbell_rang` = its sessions' rollouts grew
/// since the last read was scheduled (a turn ran somewhere on it).
pub fn read_due(
    last_read_ms: u64,
    in_flight: bool,
    unsupported: bool,
    doorbell_rang: bool,
    blocked: bool,
    now_ms: u64,
) -> bool {
    if in_flight || unsupported {
        return false;
    }
    if last_read_ms == 0 {
        return true;
    }
    let since = now_ms.saturating_sub(last_read_ms);
    if doorbell_rang && since >= READ_MIN_GAP_MS {
        return true;
    }
    since >= if blocked { READ_BLOCKED_EVERY_MS } else { READ_IDLE_EVERY_MS }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/../../fixtures/codex/0.155.1/app-server/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let reply: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        reply["result"].clone()
    }

    #[test]
    fn a_member_seat_at_99_percent_is_allowed() {
        let a = parse_account_limits(&fixture("rate_limits_member_allowed"), 1).unwrap();
        assert_eq!(a.usage_allowed, Some(true));
        assert_eq!(a.reached, None);
        assert_eq!(
            a.windows,
            vec![LimitWindow { used_percent: 99.0, window_mins: Some(10080), resets_at: Some(1790737070) }]
        );
        assert!(!a.is_blocked(false));
        // the backend's "allowed" is recovery, whatever we believed before.
        assert!(!a.is_blocked(true));
    }

    /// The block no rollout-per-session reading could see: the workspace owner
    /// is out of credits. Two of that account's sessions showed no limit at all.
    #[test]
    fn an_owner_out_of_credits_is_blocked() {
        let a = parse_account_limits(&fixture("rate_limits_owner_credits_depleted"), 1).unwrap();
        assert_eq!(a.usage_allowed, Some(false));
        assert_eq!(a.reached.as_deref(), Some("workspace_owner_credits_depleted"));
        assert!(a.is_blocked(false));
    }

    #[test]
    fn a_null_verdict_never_reads_as_recovery() {
        let mut a = parse_account_limits(&fixture("rate_limits_member_allowed"), 1).unwrap();
        a.usage_allowed = None;
        // 99%, no reason: not blocked on its own…
        assert!(!a.is_blocked(false));
        // …but it cannot LIFT a block we already hold.
        assert!(a.is_blocked(true));
        // a reached reason or a full window still blocks with no verdict.
        a.reached = Some("rate_limit_reached".into());
        assert!(a.is_blocked(false));
        a.reached = None;
        a.windows[0].used_percent = 100.0;
        assert!(a.is_blocked(false));
    }

    #[test]
    fn the_single_bucket_view_is_read_when_there_is_no_by_id_map() {
        let mut r = fixture("rate_limits_member_allowed");
        r.as_object_mut().unwrap().remove("rateLimitsByLimitId");
        let a = parse_account_limits(&r, 1).unwrap();
        assert_eq!(a.windows.len(), 1);
        // a result that says nothing is not a reading.
        assert_eq!(parse_account_limits(&json!({"rateLimits": null}), 1), None);
    }

    #[test]
    fn reads_follow_the_doorbell_and_otherwise_the_clock() {
        const T: u64 = 1_000_000_000;
        // first sight reads at once.
        assert!(read_due(0, false, false, false, false, T));
        // never two at once, never for a codex that can't answer.
        assert!(!read_due(0, true, false, true, false, T));
        assert!(!read_due(0, false, true, true, false, T));
        // a turn ran: read — but not faster than the gap.
        assert!(!read_due(T, false, false, true, false, T + READ_MIN_GAP_MS - 1));
        assert!(read_due(T, false, false, true, false, T + READ_MIN_GAP_MS));
        // quiet account: every 5 minutes; blocked: every minute.
        assert!(!read_due(T, false, false, false, false, T + READ_IDLE_EVERY_MS - 1));
        assert!(read_due(T, false, false, false, false, T + READ_IDLE_EVERY_MS));
        assert!(read_due(T, false, false, false, true, T + READ_BLOCKED_EVERY_MS));
    }

    /// The client against a stand-in app-server (a shell script, never codex):
    /// the handshake, the answer, and the one error that means "can't".
    #[test]
    fn the_client_speaks_the_handshake_and_classifies_errors() {
        let dir = std::env::temp_dir().join(format!("kod-fake-app-server-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = |name: &str, reply: &str| {
            let p = dir.join(name);
            // `$1` is "app-server"; answer initialize, swallow `initialized`, answer the read.
            std::fs::write(
                &p,
                format!(
                    "#!/bin/sh\n[ \"$1\" = app-server ] || exit 3\nread l\necho '{{\"id\":1,\"result\":{{}}}}'\nread l\nread l\necho '{reply}'\nread l\n"
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.to_string_lossy().into_owned()
        };
        let ok = fake("ok", r#"{"id":2,"result":{"ordinaryUsageAllowed":false}}"#);
        let got = read_account_limits(&ok, &dir, Duration::from_secs(5)).unwrap();
        assert_eq!(got["ordinaryUsageAllowed"], json!(false));

        let old = fake("old", r#"{"id":2,"error":{"code":-32601,"message":"method not found"}}"#);
        assert!(matches!(
            read_account_limits(&old, &dir, Duration::from_secs(5)),
            Err(ReadError::Unsupported(_))
        ));
        let auth = fake("auth", r#"{"id":2,"error":{"code":-32000,"message":"not signed in"}}"#);
        assert_eq!(
            read_account_limits(&auth, &dir, Duration::from_secs(5)),
            Err(ReadError::Failed("not signed in".into()))
        );
        assert!(matches!(
            read_account_limits("/nonexistent/codex", &dir, Duration::from_secs(1)),
            Err(ReadError::Failed(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
