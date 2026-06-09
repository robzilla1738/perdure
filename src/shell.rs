//! Real local command execution — the one place in Tach that spawns a process.
//!
//! Everywhere else, Tach is a hermetic simulation: it works an in-memory
//! workspace and calls fake, side-effect-free tools. The coding harness is the
//! deliberate exception — to verify an external agent's edits it must run the
//! project's real test command. This module is the narrow, audited gate for that:
//!
//!   * **No shell.** A command string is tokenized here (whitespace + double
//!     quotes) and the program is spawned directly. There is no `/bin/sh -c`, so
//!     pipes, redirects, `&&`, and globbing are not interpreted — the allowlist a
//!     goal grants is meaningful because the exact program is what runs.
//!   * **Fixed cwd.** The caller pins the working directory (the repo root);
//!     never a path derived from agent input.
//!   * **Scrubbed env.** The child starts from an empty environment with only a
//!     small allowlist re-inserted (`PATH`, `HOME`, …), so secrets in the parent
//!     process env (API keys an agent might hold) never leak into a subprocess or
//!     a captured artifact.
//!   * **Bounded.** A timeout kills the process group's child if it overruns.
//!   * **Captured.** stdout and stderr stream to artifact files as the process
//!     runs, drained on their own threads so a chatty `cargo test` can never
//!     deadlock by filling a pipe buffer while we wait.
//!
//! The result is turned into a durable [`crate::store::Receipt`] by the caller;
//! this module owns process mechanics only and knows nothing about the store.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A request to run one command. Everything the executor needs and nothing it
/// doesn't — the caller supplies the cwd, the timeout, and where to put artifacts.
pub struct ShellRequest<'a> {
    /// The exact command string, e.g. `"cargo test"`. Tokenized, not shell-parsed.
    pub command: &'a str,
    /// The working directory the child runs in (always the repo root).
    pub cwd: &'a Path,
    /// Wall-clock limit in milliseconds. `0` means no limit.
    pub timeout_ms: u64,
    /// Directory the stdout/stderr artifact files are written into.
    pub artifact_dir: &'a Path,
    /// A stable key (the receipt's idempotency key) used to name the artifacts, so
    /// the same command in the same run always writes the same paths.
    pub key: &'a str,
    /// The `HOME` the child is given — a sandbox dir, never the real user home, so
    /// a command can't read `~/.npmrc`, `~/.netrc`, `~/.cargo/credentials`, etc.
    /// Created if absent.
    pub home: &'a Path,
}

/// The outcome of a run. `exit_code` is `None` when the child was killed (e.g. by
/// the timeout) and so carries no code. All of the nondeterministic evidence
/// (code, duration, byte counts, captured bytes) lives here — it is recorded on a
/// receipt, never folded into the deterministic event log.
pub struct ShellResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// Absolute paths to the captured streams.
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// The tokenized argv actually spawned (argv[0] is the program).
    pub argv: Vec<String>,
    /// The resolved absolute path of the program that was run, if it could be found
    /// (argv[0] resolved against the child's `PATH`, or used as-is when it already
    /// contains a separator). Recorded on the receipt so an audit can see *which*
    /// binary ran, not just its name. `None` when it could not be resolved.
    pub program_path: Option<String>,
}

/// The environment variable names passed through to a child; everything else is
/// cleared. Names only — values are read from the parent at spawn time and never
/// recorded, so a receipt can list *what* was allowed through without leaking it.
pub fn allowed_env_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &[
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "SystemRoot",
            "USERPROFILE",
            "TEMP",
            "TMP",
        ]
    }
    #[cfg(not(windows))]
    {
        &["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"]
    }
}

/// The names whose *values* are taken from the parent, never `HOME`/`USERPROFILE`
/// — those are forced to the sandbox home so a child can't read credentials out of
/// the real user home (`~/.npmrc`, `~/.netrc`, `~/.cargo/credentials`, …).
fn parent_passthrough(name: &str) -> bool {
    !matches!(name, "HOME" | "USERPROFILE")
}

/// The redacted environment a child is given: the allowlisted parent values, with
/// `HOME` (and `USERPROFILE` on Windows) overridden to the sandbox `home`. The
/// passed-through *names* are still reported on the receipt via
/// [`allowed_env_names`]; no value is ever recorded.
pub fn child_env(home: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = allowed_env_names()
        .iter()
        .filter(|name| parent_passthrough(name))
        .filter_map(|name| std::env::var(name).ok().map(|v| (name.to_string(), v)))
        .collect();
    let home = home.to_string_lossy().into_owned();
    env.push(("HOME".to_string(), home.clone()));
    #[cfg(windows)]
    env.push(("USERPROFILE".to_string(), home));
    #[cfg(not(windows))]
    let _ = home;
    env
}

/// Best-effort resolution of `program` (argv[0]) to an absolute path: used as-is
/// when it already contains a path separator, otherwise searched on `path` (the
/// child's `PATH`). `None` when no readable candidate is found — purely advisory
/// audit metadata, never a gate.
fn resolve_program(program: &str, path: Option<&str>) -> Option<String> {
    let p = Path::new(program);
    if p.components().count() > 1 || p.is_absolute() {
        return p
            .canonicalize()
            .ok()
            .map(|c| c.to_string_lossy().into_owned());
    }
    for dir in std::env::split_paths(path.unwrap_or("")) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(program);
        if cand.is_file() {
            return cand
                .canonicalize()
                .ok()
                .map(|c| c.to_string_lossy().into_owned());
        }
    }
    None
}

/// Split a command string into argv. Whitespace separates tokens; double quotes
/// group a token containing spaces; a backslash inside quotes escapes the next
/// character. Deliberately *not* a shell: there is no variable expansion, no
/// globbing, no operator (`|`, `&&`, `>`, `;`) interpretation — those characters
/// are ordinary token bytes, which keeps the allowlist exact.
pub fn tokenize(command: &str) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut in_quote = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            match c {
                '\\' => {
                    if let Some(&next) = chars.peek() {
                        cur.push(next);
                        chars.next();
                    } else {
                        cur.push('\\');
                    }
                }
                '"' => in_quote = false,
                other => cur.push(other),
            }
            in_word = true;
        } else if c == '"' {
            in_quote = true;
            in_word = true;
        } else if c.is_whitespace() {
            if in_word {
                argv.push(std::mem::take(&mut cur));
                in_word = false;
            }
        } else {
            cur.push(c);
            in_word = true;
        }
    }
    if in_quote {
        return Err("unterminated quote in command".into());
    }
    if in_word {
        argv.push(cur);
    }
    if argv.is_empty() {
        return Err("empty command".into());
    }
    Ok(argv)
}

/// Run one command to completion (or to its timeout), capturing both streams to
/// artifact files. Pure process mechanics: no store, no events.
pub fn run(req: &ShellRequest) -> io::Result<ShellResult> {
    let argv = tokenize(req.command).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    fs::create_dir_all(req.artifact_dir)?;
    // The sandbox home must exist before the child runs (tools may write into it).
    fs::create_dir_all(req.home)?;
    let stdout_path = req.artifact_dir.join(format!("{}.stdout", req.key));
    let stderr_path = req.artifact_dir.join(format!("{}.stderr", req.key));

    let env = child_env(req.home);
    let child_path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone());
    let program_path = resolve_program(&argv[0], child_path.as_deref());

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(req.cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Put the child in its own process group (setpgid(0,0): the child becomes the
    // group leader, so its pgid equals its pid). On a timeout we then signal the
    // whole group, reaching descendants the child spawned — without this, a
    // grandchild that inherited the stdout/stderr pipes would keep them open and
    // wedge the drain threads after we kill only the direct child.
    #[cfg(unix)]
    cmd.process_group(0);

    let start = Instant::now();
    let mut child = cmd.spawn()?;

    // Drain both pipes on their own threads, straight to the artifact files. This
    // is what prevents a deadlock: a child that fills the 64KB stdout pipe buffer
    // would block forever if we only `wait()`ed without reading.
    let out = child.stdout.take();
    let err = child.stderr.take();
    let out_handle = out.map(|r| drain(r, stdout_path.clone()));
    let err_handle = err.map(|r| drain(r, stderr_path.clone()));

    // Poll for exit against the deadline. Sleeping briefly between polls keeps this
    // cheap without a platform-specific `waitpid`-with-timeout dependency.
    let deadline = (req.timeout_ms > 0).then(|| start + Duration::from_millis(req.timeout_ms));
    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(s) => break Some(s),
            None => {
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        kill_group(&mut child);
                        timed_out = true;
                        break None;
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    };

    // Killing the whole group (not just the direct child) closes every inherited
    // pipe write-end, so the drain threads see EOF and finish promptly.
    let stdout_bytes = join_drain(out_handle)?;
    let stderr_bytes = join_drain(err_handle)?;
    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(ShellResult {
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        duration_ms,
        stdout_path,
        stderr_path,
        stdout_bytes,
        stderr_bytes,
        argv,
        program_path,
    })
}

/// Terminate a timed-out child and every descendant it spawned. On Unix the child
/// leads its own process group (see [`run`]), so signaling the negative pgid reaches
/// the whole tree; closing their inherited pipe write-ends is what lets the drain
/// threads reach EOF instead of blocking on a surviving grandchild. Elsewhere we can
/// only reach the direct child. Either way we reap it so it doesn't linger as a zombie.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Group leader's pgid == its pid (process_group(0) at spawn).
        let pgid = child.id() as libc::pid_t;
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

type DrainHandle = thread::JoinHandle<io::Result<u64>>;

/// Spawn a thread that copies `reader` into `path`, returning the byte count.
fn drain(mut reader: impl Read + Send + 'static, path: PathBuf) -> DrainHandle {
    thread::spawn(move || {
        let mut w = BufWriter::new(File::create(&path)?);
        let mut buf = [0u8; 8192];
        let mut total = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])?;
            total += n as u64;
        }
        w.flush()?;
        Ok(total)
    })
}

/// Join a drain thread; if it was never spawned (no pipe), the stream is empty.
fn join_drain(handle: Option<DrainHandle>) -> io::Result<u64> {
    match handle {
        Some(h) => h
            .join()
            .map_err(|_| io::Error::other("output capture thread panicked"))?,
        None => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("tach_sh_{}_{}_{}", std::process::id(), tag, n));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tokenize_basics() {
        assert_eq!(tokenize("cargo test").unwrap(), vec!["cargo", "test"]);
        assert_eq!(
            tokenize("go test ./...").unwrap(),
            vec!["go", "test", "./..."]
        );
        assert_eq!(
            tokenize("sh \"my script.sh\"").unwrap(),
            vec!["sh", "my script.sh"]
        );
        // Operators are literal bytes, not shell syntax.
        assert_eq!(
            tokenize("echo a|b").unwrap(),
            vec!["echo", "a|b"],
            "pipe must not split a token"
        );
        assert!(tokenize("   ").is_err(), "empty command rejected");
        assert!(
            tokenize("sh \"oops").is_err(),
            "unterminated quote rejected"
        );
    }

    #[test]
    fn captures_stdout_and_exit_zero() {
        let dir = TempDir::new("ok");
        let r = run(&ShellRequest {
            command: "sh -c \"printf hello\"",
            cwd: dir.path(),
            timeout_ms: 10_000,
            artifact_dir: &dir.path().join("artifacts"),
            home: &dir.path().join("home"),
            key: "k1",
        })
        .unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert!(!r.timed_out);
        let out = fs::read_to_string(&r.stdout_path).unwrap();
        assert_eq!(out, "hello");
        assert_eq!(r.stdout_bytes, 5);
    }

    #[test]
    fn nonzero_exit_is_reported() {
        let dir = TempDir::new("fail");
        let r = run(&ShellRequest {
            command: "sh -c \"exit 3\"",
            cwd: dir.path(),
            timeout_ms: 10_000,
            artifact_dir: &dir.path().join("artifacts"),
            home: &dir.path().join("home"),
            key: "k2",
        })
        .unwrap();
        assert_eq!(r.exit_code, Some(3));
    }

    #[test]
    fn large_output_does_not_deadlock() {
        // Far more than a pipe buffer; the drain threads must keep it flowing.
        let dir = TempDir::new("big");
        let r = run(&ShellRequest {
            command: "sh -c \"yes x | head -c 2000000\"",
            cwd: dir.path(),
            timeout_ms: 20_000,
            artifact_dir: &dir.path().join("artifacts"),
            home: &dir.path().join("home"),
            key: "k3",
        })
        .unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stdout_bytes, 2_000_000);
    }

    #[test]
    fn timeout_kills_a_runaway() {
        let dir = TempDir::new("slow");
        let r = run(&ShellRequest {
            command: "sh -c \"sleep 30\"",
            cwd: dir.path(),
            timeout_ms: 300,
            artifact_dir: &dir.path().join("artifacts"),
            home: &dir.path().join("home"),
            key: "k4",
        })
        .unwrap();
        assert!(r.timed_out, "should have timed out");
        assert_eq!(r.exit_code, None);
        assert!(r.duration_ms < 10_000, "killed promptly, not after 30s");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_the_whole_tree() {
        // Decisive regression test for the process-group fix: the direct child
        // (outer sh) spawns a *grandchild* (inner sh) that would touch `marker`
        // after 2s. A timeout that killed only the direct child would leave the
        // grandchild alive — it would run to completion and create the marker
        // (and, separately, wedge the drain threads on the inherited pipe). The
        // group kill must terminate the grandchild too, so the marker never
        // appears even after we wait well past its delay.
        let dir = TempDir::new("tree");
        let marker = dir.path().join("marker");
        let cmd = format!("sh -c \"sh -c 'sleep 2; : > {}'\"", marker.display());
        let r = run(&ShellRequest {
            command: &cmd,
            cwd: dir.path(),
            timeout_ms: 300,
            artifact_dir: &dir.path().join("artifacts"),
            home: &dir.path().join("home"),
            key: "k4b",
        })
        .unwrap();
        assert!(r.timed_out, "should have timed out");
        assert!(
            r.duration_ms < 10_000,
            "drain threads unblocked promptly (grandchild's pipe was closed)"
        );
        // Wait past the grandchild's 2s delay; if it survived, the marker appears.
        thread::sleep(Duration::from_millis(2_500));
        assert!(
            !marker.exists(),
            "grandchild outlived the timeout — only the direct child was killed"
        );
    }

    #[test]
    fn env_is_scrubbed() {
        // A secret in the parent env must not reach the child.
        std::env::set_var("TACH_TEST_SECRET", "leaked");
        let dir = TempDir::new("env");
        let r = run(&ShellRequest {
            command: "sh -c \"printf %s ${TACH_TEST_SECRET:-CLEAN}\"",
            cwd: dir.path(),
            timeout_ms: 10_000,
            artifact_dir: &dir.path().join("artifacts"),
            home: &dir.path().join("home"),
            key: "k5",
        })
        .unwrap();
        std::env::remove_var("TACH_TEST_SECRET");
        let out = fs::read_to_string(&r.stdout_path).unwrap();
        assert_eq!(out, "CLEAN", "secret leaked into child env");
    }

    #[test]
    fn home_is_sandboxed_and_program_resolved() {
        // The child's HOME is the sandbox dir we pass, never the real user home —
        // so it cannot read credentials out of `~`. And the resolved program path
        // is recorded for audit.
        let dir = TempDir::new("home");
        let sandbox = dir.path().join("home");
        let r = run(&ShellRequest {
            command: "sh -c \"printf %s $HOME\"",
            cwd: dir.path(),
            timeout_ms: 10_000,
            artifact_dir: &dir.path().join("artifacts"),
            key: "k6",
            home: &sandbox,
        })
        .unwrap();
        let out = fs::read_to_string(&r.stdout_path).unwrap();
        assert_eq!(
            out,
            sandbox.to_string_lossy(),
            "child HOME must be the sandbox, not the real home"
        );
        assert!(
            sandbox.exists(),
            "sandbox home is created before the child runs"
        );
        let prog = r.program_path.expect("sh resolved on PATH");
        assert!(
            Path::new(&prog).is_absolute(),
            "resolved program path is absolute: {prog}"
        );
    }
}
