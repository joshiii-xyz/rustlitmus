//! Adapters for Miri in its two concurrency modes:
//!
//! * **weak-memory emulation** (`-Zmiri-seed=N`, randomised store buffers): the
//!   rendered program is run once per seed with `iters` rounds; the union of observed
//!   outcomes is a *sample* (`exhaustive = false`).
//! * **GenMC mode** (`-Zmiri-genmc`): requires a Miri built with `--features=genmc`; the
//!   rendered program is run for a single round and GenMC enumerates every execution the
//!   RC11-style model allows (`exhaustive = true`, with the documented exclusions).
//!
//! Both are driven through the `miri` driver binary directly (not `cargo miri`) so that
//! the exact sysroot and flags are recorded.

use crate::litmus::{Outcome, OutcomeSet};
use crate::process::{run, RunSpec};
use crate::render_rust::parse_histogram;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiriConfig {
    /// Path to the `miri` driver binary.
    pub driver: PathBuf,
    /// `--sysroot` for the Miri-built standard library.
    pub sysroot: PathBuf,
    /// Extra `-Zmiri-*` flags applied to every run.
    pub extra_flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiriRun {
    pub seed: Option<u64>,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
    #[serde(with = "crate::litmus::counts_serde")]
    pub counts: BTreeMap<Outcome, u64>,
    /// Miri reported UB / an error (not a normal exit).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiriResult {
    pub tool: String,
    pub mode: String,
    pub version: String,
    pub runs: Vec<MiriRun>,
    pub outcomes: Option<OutcomeSet>,
    #[serde(with = "crate::litmus::counts_serde")]
    pub counts: BTreeMap<Outcome, u64>,
    pub warnings: Vec<String>,
    pub assumptions: Vec<String>,
    /// GenMC mode: number of executions explored, parsed from its summary line.
    pub explored_executions: Option<u64>,
}

fn excerpt(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…[{} bytes truncated]", &s[..n], s.len() - n)
    }
}

pub fn miri_version(cfg: &MiriConfig) -> String {
    match run(&RunSpec::new(&cfg.driver, ["--version"]).timeout(Duration::from_secs(30))) {
        Ok(o) if o.exit_code == Some(0) => o.stdout.trim().to_string(),
        Ok(o) => format!("unknown ({})", o.stderr.lines().next().unwrap_or("").trim()),
        Err(e) => format!("unavailable: {e}"),
    }
}

fn base_args(cfg: &MiriConfig, source: &Path, out_dir: &Path, isolation: bool) -> Vec<String> {
    let mut a = vec!["--sysroot".to_string(), cfg.sysroot.display().to_string()];
    if !isolation {
        a.push("-Zmiri-disable-isolation".to_string());
    }
    a.extend(["-Copt-level=0".to_string(), "--out-dir".to_string(), out_dir.display().to_string(), source.display().to_string()]);
    a.extend(cfg.extra_flags.iter().cloned());
    a
}

/// Run under weak-memory emulation for each seed.
pub fn run_weak_memory(cfg: &MiriConfig, source: &Path, out_dir: &Path, seeds: &[u64], iters: usize, timeout: Duration) -> MiriResult {
    std::fs::create_dir_all(out_dir).ok();
    let version = miri_version(cfg);
    let mut runs = Vec::new();
    let mut counts: BTreeMap<Outcome, u64> = BTreeMap::new();
    let mut warnings = Vec::new();
    for &seed in seeds {
        let mut args = vec![format!("-Zmiri-seed={seed}")];
        args.extend(base_args(cfg, source, out_dir, false));
        args.push("--".into());
        args.push(iters.to_string());
        let spec = RunSpec::new(&cfg.driver, args.iter().map(String::as_str)).timeout(timeout).cwd(out_dir);
        let command = spec.command_line();
        match run(&spec) {
            Ok(o) => {
                let mut error = None;
                let mut c = BTreeMap::new();
                if o.exit_code == Some(0) && !o.timed_out {
                    match parse_histogram(&o.stdout) {
                        Ok(h) => {
                            for (k, v) in h {
                                *c.entry(k.clone()).or_insert(0) += v;
                                *counts.entry(k).or_insert(0) += v;
                            }
                        }
                        Err(e) => {
                            warnings.push(format!("seed {seed}: {e}"));
                            error = Some(e);
                        }
                    }
                } else if o.timed_out {
                    error = Some("timed out".into());
                    warnings.push(format!("seed {seed}: timed out after {:?}", timeout));
                } else {
                    let msg = o.stderr.lines().find(|l| l.contains("error")).unwrap_or("non-zero exit").to_string();
                    warnings.push(format!("seed {seed}: {msg}"));
                    error = Some(msg);
                }
                runs.push(MiriRun { seed: Some(seed), command, exit_code: o.exit_code, timed_out: o.timed_out, stdout_excerpt: excerpt(&o.stdout, 4000), stderr_excerpt: excerpt(&o.stderr, 4000), counts: c, error });
            }
            Err(e) => {
                warnings.push(format!("seed {seed}: {e}"));
                runs.push(MiriRun { seed: Some(seed), command, exit_code: None, timed_out: false, stdout_excerpt: String::new(), stderr_excerpt: e.to_string(), counts: BTreeMap::new(), error: Some(e.to_string()) });
            }
        }
    }
    let any_ok = runs.iter().any(|r| r.error.is_none());
    MiriResult {
        tool: "miri".into(),
        mode: "weak-memory-emulation".into(),
        version,
        runs,
        outcomes: any_ok.then(|| OutcomeSet::from_counts(&counts, false)),
        counts,
        warnings,
        assumptions: vec![
            "Store-buffer emulation (Lidbury & Donaldson POPL'17 as adapted by Miri); randomised, not exhaustive.".into(),
            "Cannot produce load-buffering outcomes (hb ∪ rf ∪ mo kept acyclic); SC fences over-approximated as a global AcqRel RMW.".into(),
            "Known open SC-fix unsoundness (rust-lang/miri#5104) may produce C++20-forbidden SC outcomes.".into(),
        ],
        explored_executions: None,
    }
}

/// Run in GenMC mode (single round; GenMC enumerates executions).
pub fn run_genmc(cfg: &MiriConfig, source: &Path, out_dir: &Path, timeout: Duration) -> MiriResult {
    std::fs::create_dir_all(out_dir).ok();
    let version = miri_version(cfg);
    let mut args = vec!["-Zmiri-genmc".to_string(), "-Zmiri-disable-stacked-borrows".to_string(), "-Zmiri-genmc-verbose".to_string()];
    args.extend(base_args(cfg, source, out_dir, true));
    args.push("--".into());
    args.push("1".into());
    let spec = RunSpec::new(&cfg.driver, args.iter().map(String::as_str)).timeout(timeout).cwd(out_dir);
    let command = spec.command_line();
    let mut warnings = Vec::new();
    let mut counts: BTreeMap<Outcome, u64> = BTreeMap::new();
    let mut explored = None;
    let run_rec = match run(&spec) {
        Ok(o) => {
            let mut error = None;
            if o.stderr.contains("GenMC is not supported") {
                error = Some("this Miri build has no GenMC support".to_string());
                warnings.push(error.clone().unwrap());
            } else if o.timed_out {
                error = Some("timed out".into());
                warnings.push(format!("genmc: timed out after {timeout:?}"));
            } else if o.exit_code != Some(0) {
                let msg = o.stderr.lines().find(|l| l.contains("error")).unwrap_or("non-zero exit").to_string();
                error = Some(msg.clone());
                warnings.push(format!("genmc: {msg}"));
            }
            // GenMC prints the program's stdout once per explored execution; the histogram
            // lines therefore accumulate across executions.
            let hist_lines: String = o.stdout.lines().filter(|l| l.contains('\t')).map(|l| format!("{l}\n")).collect();
            for l in o.stdout.lines().chain(o.stderr.lines()) {
                if let Some(rest) = l.strip_prefix("Verification complete with ") {
                    explored = rest.split_whitespace().next().and_then(|n| n.parse().ok());
                }
                if l.contains("warning") || l.contains("WARNING") {
                    warnings.push(l.trim().to_string());
                }
            }
            if error.is_none() {
                match parse_histogram(&hist_lines) {
                    Ok(h) => counts = h,
                    Err(e) => {
                        warnings.push(format!("genmc: {e}"));
                        error = Some(e);
                    }
                }
            }
            MiriRun { seed: None, command, exit_code: o.exit_code, timed_out: o.timed_out, stdout_excerpt: excerpt(&o.stdout, 4000), stderr_excerpt: excerpt(&o.stderr, 4000), counts: counts.clone(), error }
        }
        Err(e) => {
            warnings.push(format!("genmc: {e}"));
            MiriRun { seed: None, command, exit_code: None, timed_out: false, stdout_excerpt: String::new(), stderr_excerpt: e.to_string(), counts: BTreeMap::new(), error: Some(e.to_string()) }
        }
    };
    let ok = run_rec.error.is_none();
    MiriResult {
        tool: "miri".into(),
        mode: "genmc".into(),
        version,
        outcomes: ok.then(|| OutcomeSet::from_counts(&counts, true)),
        counts,
        runs: vec![run_rec],
        warnings,
        assumptions: vec![
            "Exhaustive DPOR exploration under GenMC's RC11 checker; complete within the documented exclusions.".into(),
            "Out-of-thin-air executions excluded (po ∪ rf acyclic): load-buffering outcomes that hardware can exhibit are NOT enumerated.".into(),
            "Separate compare_exchange failure ordering not modelled: the stronger of success/failure is used.".into(),
            "compare_exchange_weak spurious failure not explored.".into(),
            "Stacked/Tree Borrows disabled (required by GenMC mode).".into(),
        ],
        explored_executions: explored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_truncates() {
        assert_eq!(excerpt("abc", 10), "abc");
        assert!(excerpt(&"x".repeat(100), 10).contains("90 bytes truncated"));
    }
}
