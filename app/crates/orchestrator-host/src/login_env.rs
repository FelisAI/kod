//! The user's login-shell environment, for a Kod that macOS launched.
//!
//! An app opened from the Dock, Finder, Spotlight or as a login item inherits
//! launchd's environment, not the user's: `PATH=/usr/bin:/bin:/usr/sbin:/sbin`
//! and nothing ~/.zprofile or ~/.zshrc exports. Kod spawns the CLIs by NAME,
//! and every session is a child of this process or of the daemon it forks
//! (which inherits this environment), so neither `claude` (~/.local/bin) nor
//! `codex` (a node script, so `node` has to be on PATH too) could be found.
//!
//! Measured 2026-10-23, after a reboot reopened Kod as a login item: the GUI
//! and its daemon both ran with launchd's PATH, every restore failed at the PTY
//! door with "No viable candidates found in PATH", and Restore all looked like a
//! dead button. A Kod started from a terminal inherits the terminal's
//! environment, which is why this was never seen in development.
//!
//! [`adopt`] asks the user's own login shell for its environment once, at
//! startup — what VS Code and Zed do — so a Kod from the Dock hosts sessions
//! exactly like a Kod from a terminal. Measured on a real zsh setup: ~50ms
//! warm, ~220ms cold.
//!
//! Everything but [`adopt`] and `read_login_env` is pure, and those two are
//! tested against stub shells, so `cargo test` never runs the user's rc files.

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Set once this process holds the user's environment. The daemon and every
/// session inherit it, so the daemon the GUI forks never runs a second shell.
pub const MARKER: &str = "KOD_LOGIN_ENV";

const START: &str = "__KOD_ENV_START__";
const END: &str = "__KOD_ENV_END__";

/// A slow rc (nvm, conda) finishes well inside this. A hung one — an rc
/// waiting on a tty it will never get — costs one launch at most this long.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Bookkeeping that describes the probe shell, not the user's environment.
const SKIP: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

/// What [`adopt`] did, for the one startup log line. Never carries a value:
/// the environment holds the user's tokens.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Adopted {
    /// A parent Kod process already adopted it (the daemon, forked by the GUI).
    Inherited,
    /// Started from a terminal, so this environment already IS the user's.
    Terminal,
    /// Read from the login shell.
    Shell { shell: String, vars: usize, ms: u64 },
    /// The shell couldn't be read; PATH got the CLIs' install dirs instead.
    Fallback(String),
}

/// Adopt the user's login-shell environment. Call it FIRST in `main`, before
/// any thread exists: `set_var` races a concurrent `getenv` in another thread.
pub fn adopt() -> Adopted {
    if std::env::var_os(MARKER).is_some() {
        return Adopted::Inherited;
    }
    let adopted = if std::io::stdin().is_terminal() {
        Adopted::Terminal
    } else {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/zsh".into());
        let t0 = Instant::now();
        match read_login_env(&shell, TIMEOUT) {
            Ok(vars) => {
                let n = vars.len();
                for (k, v) in vars {
                    std::env::set_var(k, v);
                }
                Adopted::Shell { shell, vars: n, ms: t0.elapsed().as_millis() as u64 }
            }
            Err(why) => {
                let home = std::env::var_os("HOME").unwrap_or_default();
                let path = std::env::var_os("PATH").unwrap_or_default();
                std::env::set_var("PATH", fallback_path(&path, Path::new(&home), Path::is_dir));
                Adopted::Fallback(why)
            }
        }
    };
    std::env::set_var(MARKER, "1");
    adopted
}

/// Run `shell` as a login, interactive shell and read back its environment.
///
/// `-i` as well as `-l`: PATH lines live in ~/.zshrc as often as ~/.zprofile.
/// `env` is called by absolute path because the shell's PATH is the thing being
/// read, and the markers fence off anything an rc prints first.
fn read_login_env(shell: &str, timeout: Duration) -> Result<Vec<(OsString, OsString)>, String> {
    let script = format!("printf '%s' {START}; /usr/bin/env -0; printf '%s' {END}");
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't run {shell}: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel();
    // Read up to the END marker, not to EOF: an rc that starts a background
    // agent hands it this pipe, and EOF then never comes. If such an agent
    // keeps the pipe open, this thread stays parked in read() — it never
    // touches the environment, so `adopt` may still set_var around it.
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if find(&buf, END.as_bytes()).is_some() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(buf);
    });
    let got = rx.recv_timeout(timeout);
    let _ = child.kill();
    let _ = child.wait();
    let out = got.map_err(|_| format!("{shell} didn't answer within {}s", timeout.as_secs()))?;
    parse_env(&out).ok_or_else(|| format!("{shell} printed no environment"))
}

/// The `KEY=value` pairs between the markers of `env -0` output. None when the
/// markers are missing or no PATH came back — a shell that printed something
/// else entirely.
fn parse_env(out: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    let body = &out[find(out, START.as_bytes())? + START.len()..];
    let body = &body[..find(body, END.as_bytes())?];
    let vars: Vec<(OsString, OsString)> = body
        .split(|b| *b == 0)
        .filter_map(|kv| {
            let eq = kv.iter().position(|b| *b == b'=')?;
            let (k, v) = (&kv[..eq], &kv[eq + 1..]);
            let skip = k.is_empty()
                || k == MARKER.as_bytes()
                || SKIP.iter().any(|s| s.as_bytes() == k);
            (!skip).then(|| (OsString::from_vec(k.to_vec()), OsString::from_vec(v.to_vec())))
        })
        .collect();
    vars.iter().any(|(k, _)| k == "PATH").then_some(vars)
}

/// PATH for when the shell can't be read: the dirs the CLIs' installers use
/// (claude's in ~/.local/bin, Homebrew's), each only if it exists, ahead of
/// what launchd gave us.
fn fallback_path(path: &OsStr, home: &Path, exists: impl Fn(&Path) -> bool) -> OsString {
    let mut dirs: Vec<PathBuf> = [
        home.join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]
    .into_iter()
    .filter(|d| exists(d))
    .collect();
    for d in std::env::split_paths(path) {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| path.to_os_string())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_dump(pairs: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in pairs {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    fn get<'a>(vars: &'a [(OsString, OsString)], k: &str) -> Option<&'a OsStr> {
        vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_os_str())
    }

    #[test]
    fn parses_between_the_markers_only() {
        let mut out = b"Last login: Fri\nwelcome to zsh\n".to_vec();
        out.extend_from_slice(START.as_bytes());
        out.extend(env_dump(&[
            "PATH=/Users/u/.local/bin:/usr/bin",
            "MULTI=line one\nline two",
            "EQ=a=b=c",
            "PWD=/Users/u",
            "OLDPWD=/",
            "SHLVL=1",
            "_=/usr/bin/env",
            "KOD_LOGIN_ENV=1",
        ]));
        out.extend_from_slice(END.as_bytes());
        out.extend_from_slice(b"bye\n");
        let vars = parse_env(&out).expect("parsed");
        assert_eq!(get(&vars, "PATH"), Some(OsStr::new("/Users/u/.local/bin:/usr/bin")));
        assert_eq!(get(&vars, "MULTI"), Some(OsStr::new("line one\nline two")));
        assert_eq!(get(&vars, "EQ"), Some(OsStr::new("a=b=c")));
        for k in ["PWD", "OLDPWD", "SHLVL", "_", MARKER] {
            assert_eq!(get(&vars, k), None, "{k} describes the probe, not the user");
        }
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn no_markers_or_no_path_is_no_environment() {
        assert!(parse_env(b"zsh: command not found: printf").is_none());
        let mut out = START.as_bytes().to_vec();
        out.extend(env_dump(&["HOME=/Users/u"]));
        out.extend_from_slice(END.as_bytes());
        assert!(parse_env(&out).is_none(), "no PATH came back");
        let mut cut = START.as_bytes().to_vec();
        cut.extend(env_dump(&["PATH=/usr/bin"]));
        assert!(parse_env(&cut).is_none(), "cut off before END");
    }

    #[test]
    fn fallback_puts_existing_install_dirs_first_once() {
        let home = Path::new("/Users/u");
        let launchd = OsStr::new("/usr/bin:/bin:/usr/local/bin");
        let exists = |p: &Path| p != Path::new("/opt/homebrew/bin");
        let got = fallback_path(launchd, home, exists);
        assert_eq!(got, OsString::from("/Users/u/.local/bin:/usr/local/bin:/usr/bin:/bin"));
    }

    /// A stand-in for the user's shell: prints `noise`, runs the `-c` script
    /// exactly as a real shell would, then runs `after`.
    fn stub_shell(name: &str, noise: &str, after: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kod-login-env-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sh");
        // argv is `-l -i -c <script>`, so the script is $4.
        let body = format!("#!/bin/sh\n{noise}\n/bin/sh -c \"$4\"\n{after}\n");
        std::fs::write(&path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn reads_a_shell_that_talks_first() {
        let sh = stub_shell("talks", "echo 'motd: hello'", "");
        let vars = read_login_env(sh.to_str().unwrap(), Duration::from_secs(5)).expect("read");
        assert!(get(&vars, "PATH").is_some());
    }

    #[test]
    fn a_background_agent_holding_the_pipe_does_not_stall_launch() {
        // the rc forks something long-lived that inherits stdout — EOF never
        // comes, but the END marker already did.
        let sh = stub_shell("agent", "", "sleep 30 &");
        let t0 = Instant::now();
        let vars = read_login_env(sh.to_str().unwrap(), Duration::from_secs(5)).expect("read");
        assert!(get(&vars, "PATH").is_some());
        assert!(t0.elapsed() < Duration::from_secs(3), "waited for EOF: {:?}", t0.elapsed());
    }

    #[test]
    fn a_hung_shell_times_out() {
        let sh = stub_shell("hung", "sleep 30", "");
        let t0 = Instant::now();
        let err = read_login_env(sh.to_str().unwrap(), Duration::from_millis(400)).unwrap_err();
        assert!(err.contains("didn't answer"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_missing_shell_is_an_error_not_a_panic() {
        let err = read_login_env("/nonexistent/kod-shell", Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("couldn't run"), "{err}");
    }
}
