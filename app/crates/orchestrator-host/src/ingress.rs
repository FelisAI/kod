//! Hook ingress (docs/014) — one unix socket per app instance receives Claude
//! hook payloads and routes them to the originating session by our SessionId.
//!
//! The session id is templated into each session's hook command, so routing is
//! deterministic before Claude's own session id exists. Wire format per
//! connection: a header line `"<session_id> <event>\n"` then the raw JSON body
//! to EOF. Dependency-free on macOS (the hook command is a python3 one-liner;
//! `AF_UNIX` verified present).

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crate::hooks::{parse_hook_payload, HookEvent};
use crate::session::SessionId;

/// A parsed, routed hook message.
pub struct HookMessage {
    pub session: SessionId,
    pub event: HookEvent,
}

/// Owns the listening socket + per-session settings dir. Dropping it removes
/// the socket file.
pub struct HookIngress {
    sock_path: PathBuf,
    base_dir: PathBuf,
}

impl HookIngress {
    /// Bind a fresh socket under `$TMPDIR/orchestrator-<pid>/` and start
    /// accepting; each routed message is handed to `on_message` on the accept
    /// thread (keep it cheap — push into session state).
    pub fn start(
        on_message: impl Fn(HookMessage) + Send + 'static,
    ) -> std::io::Result<Arc<HookIngress>> {
        let pid = std::process::id();
        // Fully per-instance dir so concurrent ingresses (e.g. tests) never
        // share a path AND each can remove its own tree on Drop without
        // touching a sibling. In the real app there is exactly one.
        static INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let inst = INSTANCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let base_dir = std::env::temp_dir().join(format!("orchestrator-{pid}-{inst}"));
        std::fs::create_dir_all(&base_dir)?;
        let sock_path = base_dir.join("hooks.sock");
        let _ = std::fs::remove_file(&sock_path); // stale from a prior crash
        let listener = UnixListener::bind(&sock_path)?;

        thread::Builder::new()
            .name("hook-ingress".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut buf = Vec::new();
                    if stream.read_to_end(&mut buf).is_err() {
                        continue;
                    }
                    if let Some(msg) = parse_message(&buf) {
                        on_message(msg);
                    }
                }
            })?;

        Ok(Arc::new(HookIngress { sock_path, base_dir }))
    }

    pub fn socket_path(&self) -> &Path {
        &self.sock_path
    }

    /// Write a per-session Claude settings file wiring the hooks to this socket
    /// and return its path (passed as `--settings`). The session id is baked
    /// into every hook command for deterministic routing. `effort` (from
    /// SpawnSpec) presets the session's reasoning mode — this is THE one place
    /// it's applied, for spawn and resume alike.
    pub fn write_session_settings(&self, id: SessionId, effort: &str) -> std::io::Result<PathBuf> {
        let dir = self.base_dir.join(format!("session-{}", id.0));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("settings.json");
        std::fs::write(&path, self.settings_json(id, effort))?;
        Ok(path)
    }

    /// The settings fragment for a default effort. "ultracode" is a MODE flag
    /// (xhigh + workflow orchestration), the rest are effortLevel values.
    /// Allowlisted: anything else (incl. "" and stale values like "max", which
    /// settings.json rejects) contributes nothing.
    fn effort_fragment(effort: &str) -> &'static str {
        match effort {
            "ultracode" => "\n  \"ultracode\": true,",
            "low" => "\n  \"effortLevel\": \"low\",",
            "medium" => "\n  \"effortLevel\": \"medium\",",
            "high" => "\n  \"effortLevel\": \"high\",",
            "xhigh" => "\n  \"effortLevel\": \"xhigh\",",
            _ => "",
        }
    }

    fn settings_json(&self, id: SessionId, effort: &str) -> String {
        let sock = self.sock_path.to_string_lossy();
        let cmd = |event: &str| hook_command(&sock, id, event);
        // Documented Claude hooks schema: event → [{ hooks: [{type, command}] }].
        // PermissionRequest carries the WHAT; PreToolUse/Stop/Notification are
        // activity signals for M3 (Stop clears a stale pending decision).
        format!(
            r#"{{{eff}
  "hooks": {{
    "PermissionRequest": [{{ "hooks": [{{ "type": "command", "command": {pr} }}] }}],
    "PreToolUse": [{{ "matcher": ".*", "hooks": [{{ "type": "command", "command": {pt} }}] }}],
    "Stop": [{{ "hooks": [{{ "type": "command", "command": {st} }}] }}],
    "Notification": [{{ "hooks": [{{ "type": "command", "command": {no} }}] }}]
  }}
}}"#,
            eff = Self::effort_fragment(effort),
            pr = json_str(&cmd("PermissionRequest")),
            pt = json_str(&cmd("PreToolUse")),
            st = json_str(&cmd("Stop")),
            no = json_str(&cmd("Notification")),
        )
    }
}

impl Drop for HookIngress {
    fn drop(&mut self) {
        // remove the whole instance tree (socket + every per-session settings
        // dir), not just the socket — symmetric RAII cleanup.
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_dir_all(&self.base_dir);
    }
}

/// The hook command: a python3 one-liner that frames `"<id> <event>\n"` + the
/// stdin JSON to the unix socket. The socket PATH is passed as `argv[1]`
/// (shell-single-quoted) and read via `sys.argv[1]` — never interpolated into
/// the python source — so an arbitrary path (quotes/backslashes) can't break
/// quoting or inject shell. `id` is a u64 and `event` a fixed literal, both
/// safe to embed in the framed header.
fn hook_command(sock: &str, id: SessionId, event: &str) -> String {
    format!(
        "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); \
s.connect(sys.argv[1]); s.sendall(b\"{id} {event}\\n\"); \
s.sendall(sys.stdin.buffer.read()); s.shutdown(socket.SHUT_WR)' {sock}",
        id = id.0,
        sock = shell_squote(sock),
    )
}

/// POSIX single-quote a string so the shell passes it through verbatim.
fn shell_squote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Minimal JSON string-encode for embedding a command into the settings file.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn parse_message(buf: &[u8]) -> Option<HookMessage> {
    let nl = buf.iter().position(|&b| b == b'\n')?;
    let header = std::str::from_utf8(&buf[..nl]).ok()?;
    let mut it = header.splitn(2, ' ');
    let id: u64 = it.next()?.parse().ok()?;
    let _event = it.next()?; // event name is also in the payload; classify there
    let body = &buf[nl + 1..];
    let json = std::str::from_utf8(body).ok()?;
    let event = parse_hook_payload(json).ok()?;
    Some(HookMessage { session: SessionId(id), event })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn routes_a_posted_hook_to_the_right_session() {
        let (tx, rx) = mpsc::channel();
        let ingress = HookIngress::start(move |m| tx.send(m).unwrap()).unwrap();

        // simulate the hook command: connect, frame header + JSON, close write.
        let mut s = UnixStream::connect(ingress.socket_path()).unwrap();
        let payload = br#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        s.write_all(b"42 PermissionRequest\n").unwrap();
        s.write_all(payload).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).expect("hook not routed");
        assert_eq!(msg.session, SessionId(42));
        assert!(matches!(msg.event, HookEvent::PermissionRequest(_)));
    }

    #[test]
    fn settings_file_is_valid_json_and_names_this_socket() {
        let ingress = HookIngress::start(|_| {}).unwrap();
        let path = ingress.write_session_settings(SessionId(7), "").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).expect("settings must be valid JSON");
        let cmd = v["hooks"]["PermissionRequest"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("7 PermissionRequest"), "session id not templated: {cmd}");
        assert!(cmd.contains(&ingress.socket_path().to_string_lossy().to_string()));
        // no effort requested → no effort keys in the file.
        assert!(v.get("effortLevel").is_none() && v.get("ultracode").is_none());
    }

    #[test]
    fn settings_file_carries_effort_and_ultracode() {
        let ingress = HookIngress::start(|_| {}).unwrap();
        // a plain level → effortLevel; hooks still intact.
        let path = ingress.write_session_settings(SessionId(8), "xhigh").unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["effortLevel"], "xhigh");
        assert!(v["hooks"]["Stop"][0]["hooks"][0]["command"].is_string());
        // ultracode is a MODE flag, not an effortLevel.
        let path = ingress.write_session_settings(SessionId(9), "ultracode").unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["ultracode"], true);
        assert!(v.get("effortLevel").is_none());
        // non-allowlisted (stale "max", junk) contributes nothing.
        let path = ingress.write_session_settings(SessionId(10), "max").unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v.get("effortLevel").is_none() && v.get("ultracode").is_none());
    }

    #[test]
    fn socket_file_removed_on_drop() {
        let path;
        {
            let ingress = HookIngress::start(|_| {}).unwrap();
            path = ingress.socket_path().to_path_buf();
            assert!(path.exists());
        }
        assert!(!path.exists(), "socket leaked after drop");
    }
}
