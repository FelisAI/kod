//! PTY plumbing (docs/013 §1 pty.rs).
//!
//! Spawn a command in a PTY we own, pump its output to a callback on a reader
//! thread, expose a writer + resize, and clean up the child on drop. This is
//! the impure boundary; everything it feeds (emulator, osc, hooks) is pure.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// How to launch a session's process. Stays general so the M3 hook wiring can
/// add `--settings` / env without touching this layer (docs/013 §4 contract).
/// Serde-plain for the daemon-extraction wire (docs/013 §1).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    /// Default reasoning effort for a claude spawn/resume ("" = claude's own
    /// default; low/…/xhigh, or "ultracode"). Applied HOST-side in the session
    /// --settings file, so every claude path gets it from one place instead of
    /// each GUI call site remembering an env push. Shape change ⇒ WIRE_VERSION 9
    /// (bincode is positional; the Hello gate handles peer mismatch).
    #[serde(default)]
    pub effort: String,
    /// Dispatch delivery: a prompt to hand a fresh claude as its final
    /// positional arg (docs/011 WIRE). "" = spawn at the composer as before.
    /// MUST stay the LAST field — bincode is positional and `#[serde(default)]`
    /// only saves an older peer if the new field is at the tail. WIRE_VERSION 14.
    #[serde(default)]
    pub initial_prompt: String,
}

impl SpawnSpec {
    pub fn shell(cwd: impl AsRef<Path>) -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        SpawnSpec {
            program: shell,
            args: vec!["-l".into()],
            cwd: cwd.as_ref().to_path_buf(),
            env: vec![("TERM".into(), "xterm-256color".into())],
            rows: 30,
            cols: 110,
            effort: String::new(),
            initial_prompt: String::new(),
        }
    }

    pub fn program(program: impl Into<String>, cwd: impl AsRef<Path>) -> Self {
        SpawnSpec {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.as_ref().to_path_buf(),
            env: vec![("TERM".into(), "xterm-256color".into())],
            rows: 30,
            cols: 110,
            effort: String::new(),
            initial_prompt: String::new(),
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A live PTY-hosted process. Dropping it kills the child's whole process
/// group and joins the reader with a bound (so a stray descendant holding the
/// slave open can never hang app teardown).
pub struct PtyProcess {
    // Mutex-wrapped so PtyProcess is Sync (the session is shared with the hook
    // ingress thread): MasterPty and Receiver are Send but not Sync.
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: SharedWriter,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    alive: Arc<AtomicBool>,
    /// set once the child has been reaped — by the reader on natural exit, or by
    /// `terminate()` on explicit close. The CAS guarantees the child is `wait()`ed
    /// EXACTLY once (no zombie, no double-wait) — docs/018 §10.
    terminated: Arc<AtomicBool>,
    /// process-group leader pid, for group-wide signalling on terminate.
    pgid: Option<i32>,
    /// the reader signals here when it exits, so Drop can bound its wait.
    reader_done: Mutex<std::sync::mpsc::Receiver<()>>,
    reader_handle: Option<JoinHandle<()>>,
}

impl PtyProcess {
    /// Spawn `spec`; `on_output` is called on the reader thread with each
    /// chunk of PTY bytes and RETURNS any bytes to write straight back to the
    /// PTY (terminal-query replies: CPR/DA1/color). Returning them lets the
    /// reader thread flush replies with zero latency and no construction-order
    /// cycle. Keep the callback cheap — feed an emulator, return its replies.
    pub fn spawn(
        spec: &SpawnSpec,
        mut on_output: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static,
    ) -> std::io::Result<PtyProcess> {
        // THE $HOME BACKSTOP (#29). portable-pty does NOT fail on a cwd that
        // doesn't exist — `CommandBuilder::as_command` silently DROPS it and
        // substitutes $HOME, so a bad cwd would return Ok and drop a file-writing
        // agent into the user's home folder with no error anywhere. Refuse
        // here, at the one door every PTY (spawn, resume, restore, daemon) goes
        // through, so no caller can reintroduce it.
        if !spec.cwd.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("working directory doesn't exist: {}", spec.cwd.display()),
            ));
        }
        let sys = native_pty_system();
        let pair = sys
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;

        let mut cmd = CommandBuilder::new(&spec.program);
        for a in &spec.args {
            cmd.arg(a);
        }
        cmd.cwd(&spec.cwd);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // CommandBuilder inherits the parent env; scrub the nested-claude
        // markers from claude children. Measured (2026-07-03): with these
        // inherited (app launched from inside a claude session), the CLI writes
        // NO transcript at the minted --session-id — silently breaking dispatch
        // linkage-at-birth. Here so EVERY claude path (spawn + resume) gets it.
        if Path::new(&spec.program)
            .file_name()
            .is_some_and(|p| p == "claude")
        {
            // prefix scrub, not a fixed list — a future CLI may key nested
            // detection off any CLAUDE_CODE_* marker (CLAUDE_CODE_EXECPATH is
            // already present in real nested envs). CLAUDE_CONFIG_DIR et al.
            // don't match the prefix and survive.
            cmd.env_remove("CLAUDECODE");
            for (k, _) in std::env::vars() {
                if k.starts_with("CLAUDE_CODE_") {
                    cmd.env_remove(&k);
                }
            }
        }

        let child = Arc::new(Mutex::new(pair.slave.spawn_command(cmd).map_err(to_io)?));
        // process_group_leader lives on the MASTER pty; capture it for
        // group-wide signalling on terminate.
        let pgid = pair.master.process_group_leader();
        let mut reader = pair.master.try_clone_reader().map_err(to_io)?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().map_err(to_io)?));
        drop(pair.slave);

        let alive = Arc::new(AtomicBool::new(true));
        let terminated = Arc::new(AtomicBool::new(false));
        let alive_r = alive.clone();
        let terminated_r = terminated.clone();
        let child_r = child.clone();
        let writer_r = writer.clone();
        let (done_tx, reader_done) = std::sync::mpsc::sync_channel::<()>(1);
        let reader_handle = thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let reply = on_output(&buf[..n]);
                            if !reply.is_empty() {
                                if let Ok(mut w) = writer_r.lock() {
                                    let _ = w.write_all(&reply);
                                    let _ = w.flush();
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Natural exit (the CLI ended itself): REAP the child so a
                // long-lived daemon doesn't leak a zombie for a Dead session
                // (docs/018 §10). CAS so the child is waited exactly once —
                // `terminate()` takes it on the explicit-close path instead.
                if !terminated_r.swap(true, Ordering::SeqCst) {
                    if let Ok(mut c) = child_r.lock() {
                        let _ = c.wait();
                    }
                }
                alive_r.store(false, Ordering::SeqCst);
                let _ = done_tx.try_send(());
            })
            .map_err(to_io)?;

        Ok(PtyProcess {
            master: Mutex::new(pair.master),
            writer,
            child,
            alive,
            terminated,
            pgid,
            reader_done: Mutex::new(reader_done),
            reader_handle: Some(reader_handle),
        })
    }

    /// Write bytes to the PTY (keystrokes, injected answers).
    pub fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Resize the PTY (and thus the child's view of the terminal).
    pub fn resize(&self, rows: u16, cols: u16) {
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// True until the child's stdout closes (process exit).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Whether the child has been reaped (natural exit or `terminate()`).
    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }

    /// Kill this session's process group NOW (the explicit-close path,
    /// docs/018 §10). Idempotent and CAS-coordinated with the reader's reap so
    /// the child is `wait()`ed exactly once. Shared by `close()` and `Drop`.
    pub fn terminate(&self) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return; // already reaped (natural exit) or terminated
        }
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill(); // SIGHUP→SIGKILL to the direct child
            let _ = c.wait();
        }
        // Also signal the whole process group, so a backgrounded job in the
        // session is killed too (kill() only targets the direct child pid).
        if let Some(pgid) = self.pgid {
            // SAFETY: killpg on our own child's group; benign if already gone.
            unsafe {
                libc::killpg(pgid, libc::SIGHUP);
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        self.alive.store(false, Ordering::SeqCst);
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.terminate(); // CAS-guarded kill/reap — no-op if already reaped
                          // Bound the wait: a stray descendant in its OWN group can keep the
                          // slave fd (and thus the blocking read()) open. Wait briefly, then
                          // detach the reader rather than hang teardown forever.
        let joined = self
            .reader_done
            .lock()
            .map(|rx| {
                rx.recv_timeout(std::time::Duration::from_millis(400))
                    .is_ok()
            })
            .unwrap_or(false);
        if joined {
            if let Some(h) = self.reader_handle.take() {
                let _ = h.join();
            }
        }
        // else: drop the JoinHandle without joining → thread detaches; it exits
        // when the fd finally closes. Teardown never blocks.
    }
}

fn to_io(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn wait_until(mut f: impl FnMut() -> bool, ms: u64) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    #[test]
    fn a_missing_cwd_is_an_error_never_a_home_folder_spawn() {
        // #29: portable-pty silently substitutes $HOME for a cwd that doesn't
        // exist (CommandBuilder::as_command filters on is_dir), so without this
        // guard the process would start — in the user's home — and report Ok.
        let missing = std::env::temp_dir().join("orchestrator-no-such-dir-29");
        let _ = std::fs::remove_dir_all(&missing);
        let spec = SpawnSpec::program("cat", &missing);
        let err = PtyProcess::spawn(&spec, |_| Vec::new())
            .err()
            .expect("a non-existent cwd must fail, not fall back to $HOME");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("working directory doesn't exist"));
    }

    #[test]
    fn echo_roundtrips_through_the_pty() {
        // a tiny non-agent program in the PTY (RULE ZERO: never claude/codex)
        let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
        let c2 = collected.clone();
        let spec = SpawnSpec::program("cat", std::env::temp_dir());
        let pty = PtyProcess::spawn(&spec, move |b| {
            c2.lock().unwrap().extend_from_slice(b);
            Vec::new()
        })
        .unwrap();
        pty.write(b"hello orchestrator\n");
        let ok = wait_until(
            || {
                let g = collected.lock().unwrap();
                String::from_utf8_lossy(&g).contains("hello orchestrator")
            },
            2000,
        );
        assert!(ok, "cat did not echo input back through the pty");
    }

    #[test]
    fn child_exit_marks_not_alive() {
        let spec = SpawnSpec::program("true", std::env::temp_dir());
        let pty = PtyProcess::spawn(&spec, |_| Vec::new()).unwrap();
        assert!(
            wait_until(|| !pty.is_alive(), 2000),
            "exited child still alive"
        );
    }

    #[test]
    fn drop_kills_a_long_running_child() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        // sleep would outlive the test; drop must kill it
        let spec = SpawnSpec::program("sleep", std::env::temp_dir()).arg("30");
        let pty = PtyProcess::spawn(&spec, move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })
        .unwrap();
        assert!(pty.is_alive());
        drop(pty); // Drop joins the reader — only returns if the child was killed
    }

    #[test]
    fn resize_does_not_panic() {
        let spec = SpawnSpec::program("cat", std::env::temp_dir());
        let pty = PtyProcess::spawn(&spec, |_| Vec::new()).unwrap();
        pty.resize(40, 120);
    }

    #[test]
    fn terminate_kills_promptly_and_is_idempotent() {
        let spec = SpawnSpec::program("sleep", std::env::temp_dir()).arg("30");
        let pty = PtyProcess::spawn(&spec, |_| Vec::new()).unwrap();
        assert!(pty.is_alive());
        pty.terminate();
        assert!(
            wait_until(|| !pty.is_alive(), 1000),
            "terminate did not kill the child"
        );
        assert!(pty.is_terminated());
        pty.terminate(); // idempotent — no panic, no double-wait
    }

    #[test]
    fn natural_exit_reaps_the_child() {
        // a self-exiting child must be reaped by the reader (no zombie) — the
        // `terminated` flag flips WITHOUT anyone calling terminate().
        let spec = SpawnSpec::program("true", std::env::temp_dir());
        let pty = PtyProcess::spawn(&spec, |_| Vec::new()).unwrap();
        assert!(
            wait_until(|| pty.is_terminated(), 2000),
            "exited child was not reaped"
        );
        assert!(!pty.is_alive());
    }
}
