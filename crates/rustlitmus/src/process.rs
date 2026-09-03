//! Bounded child-process execution.
//!
//! Every external tool (rustc, herd7, miri, generated test binaries) is run through
//! [`run`], which enforces a wall-clock timeout, kills the whole process on expiry,
//! captures stdout/stderr, and never inherits the parent's environment wholesale:
//! only an explicit allow-list of variables is passed, which is also what keeps
//! credentials out of evidence bundles.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    /// Maximum bytes of stdout/stderr retained (each). Anything beyond is truncated and
    /// flagged so a runaway child cannot exhaust memory or disk.
    pub max_output: usize,
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
    pub wall: Duration,
}

#[derive(Debug)]
pub enum RunError {
    Spawn { program: PathBuf, source: std::io::Error },
    Wait(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn { program, source } => write!(f, "failed to spawn {}: {source}", program.display()),
            RunError::Wait(e) => write!(f, "failed waiting for child: {e}"),
        }
    }
}
impl std::error::Error for RunError {}

/// Environment variables forwarded to children. Deliberately small: toolchain
/// discovery plus locale. Anything that could carry a credential (tokens, `*_KEY`,
/// `*_SECRET`, cloud config) is never forwarded.
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "RUSTUP_HOME", "CARGO_HOME", "OPAMROOT", "LD_LIBRARY_PATH"];

impl RunSpec {
    pub fn new<I, S>(program: impl AsRef<Path>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut env = BTreeMap::new();
        for k in ENV_ALLOWLIST {
            if let Ok(v) = std::env::var(k) {
                env.insert(k.to_string(), v);
            }
        }
        RunSpec {
            program: program.as_ref().to_path_buf(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env,
            timeout: Duration::from_secs(120),
            max_output: 16 * 1024 * 1024,
        }
    }
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
    pub fn cwd(mut self, d: impl AsRef<Path>) -> Self {
        self.cwd = Some(d.as_ref().to_path_buf());
        self
    }
    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    pub fn max_output(mut self, n: usize) -> Self {
        self.max_output = n;
        self
    }
    /// Human-readable command line for evidence bundles.
    pub fn command_line(&self) -> Vec<String> {
        let mut v = vec![self.program.display().to_string()];
        v.extend(self.args.iter().map(|a| a.to_string_lossy().into_owned()));
        v
    }
}

fn drain(mut r: impl Read + Send + 'static, cap: usize) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut truncated = false;
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < cap {
                        let take = n.min(cap - buf.len());
                        buf.extend_from_slice(&chunk[..take]);
                        if take < n {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        (buf, truncated)
    })
}

pub fn run(spec: &RunSpec) -> Result<RunOutput, RunError> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args).env_clear().envs(&spec.env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = &spec.cwd {
        cmd.current_dir(d);
    }
    // Put the child in its own process group so that a timeout kills its whole tree
    // (e.g. `sh -c` wrappers, rustc's linker, cargo's children), not just the leader.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| RunError::Spawn { program: spec.program.clone(), source: e })?;
    let out = drain(child.stdout.take().expect("piped"), spec.max_output);
    let err = drain(child.stderr.take().expect("piped"), spec.max_output);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(RunError::Wait)? {
            Some(s) => break Some(s),
            None => {
                if start.elapsed() >= spec.timeout {
                    kill_tree(&mut child);
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    let (so, t1) = out.join().unwrap_or_default();
    let (se, t2) = err.join().unwrap_or_default();
    Ok(RunOutput {
        exit_code: status.and_then(|s| s.code()),
        stdout: String::from_utf8_lossy(&so).into_owned(),
        stderr: String::from_utf8_lossy(&se).into_owned(),
        timed_out,
        truncated: t1 || t2,
        wall: start.elapsed(),
    })
}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` with a negative pid signals the process group; the group id
        // equals the child's pid because we spawned it with `process_group(0)`.
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe {
            kill(-(child.id() as i32), 9);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Locate a program on `PATH` (or accept an absolute path).
pub fn which(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_captures() {
        let o = run(&RunSpec::new("/bin/sh", ["-c", "echo out; echo err 1>&2; exit 3"])).unwrap();
        assert_eq!(o.exit_code, Some(3));
        assert_eq!(o.stdout, "out\n");
        assert_eq!(o.stderr, "err\n");
        assert!(!o.timed_out);
    }

    #[test]
    fn enforces_timeout() {
        let o = run(&RunSpec::new("/bin/sh", ["-c", "sleep 30"]).timeout(Duration::from_millis(200))).unwrap();
        assert!(o.timed_out);
        assert!(o.exit_code.is_none());
        assert!(o.wall < Duration::from_secs(5));
    }

    #[test]
    fn truncates_output() {
        let o = run(&RunSpec::new("/bin/sh", ["-c", "head -c 100000 /dev/zero | tr '\\0' 'a'"]).max_output(1000)).unwrap();
        assert!(o.truncated);
        assert_eq!(o.stdout.len(), 1000);
    }

    #[test]
    fn does_not_leak_environment() {
        std::env::set_var("RUSTLITMUS_TEST_SECRET_TOKEN", "hunter2");
        let o = run(&RunSpec::new("/bin/sh", ["-c", "env"])).unwrap();
        assert!(!o.stdout.contains("hunter2"));
        std::env::remove_var("RUSTLITMUS_TEST_SECRET_TOKEN");
    }

    #[test]
    fn spawn_error_is_reported() {
        assert!(run(&RunSpec::new("/nonexistent/binary/xyz", Vec::<String>::new())).is_err());
    }
}
