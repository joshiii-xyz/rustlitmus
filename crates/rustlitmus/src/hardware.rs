//! Hardware execution of a compiled litmus binary, with environment capture.
//!
//! The binary is the renderer's harness; it prints an outcome histogram. We run it in
//! fresh processes (one per repetition batch) under the bounded runner, optionally under
//! a user-mode emulator for foreign targets—in which case the result is explicitly
//! labelled `emulated` and is *not* hardware evidence.

use crate::litmus::{Outcome, OutcomeSet};
use crate::process::{run, RunSpec};
use crate::render_rust::parse_histogram;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub arch: String,
    pub os: String,
    pub kernel: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_vendor: Option<String>,
    pub cpu_flags_excerpt: Option<String>,
    pub cpus_online: Option<usize>,
    pub hypervisor: Option<String>,
    pub container_hint: Option<String>,
    pub microcode: Option<String>,
}

pub fn host_info() -> HostInfo {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    let cpuinfo = read("/proc/cpuinfo").unwrap_or_default();
    let field = |key: &str| cpuinfo.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string());
    let flags = field("flags").or_else(|| field("Features"));
    let hypervisor = flags.as_deref().and_then(|f| f.split_whitespace().any(|x| x == "hypervisor").then(|| "cpuid hypervisor bit set".to_string()));
    let kernel = read("/proc/version").map(|v| v.trim().to_string());
    let container_hint = read("/proc/1/cgroup").and_then(|c| {
        let l = c.lines().next()?.to_string();
        (l.contains("docker") || l.contains("containerd") || l.contains("/ta-") || l.contains("kubepods")).then_some(l)
    });
    HostInfo {
        arch: std::env::consts::ARCH.into(),
        os: std::env::consts::OS.into(),
        kernel,
        cpu_model: field("model name"),
        cpu_vendor: field("vendor_id").or_else(|| field("CPU implementer")),
        cpu_flags_excerpt: flags.map(|f| {
            let keep: Vec<&str> = f.split_whitespace().filter(|x| ["sse2", "avx", "avx2", "avx512f", "hypervisor", "lse", "atomics", "rcpc"].contains(x)).collect();
            keep.join(" ")
        }),
        cpus_online: std::thread::available_parallelism().ok().map(|n| n.get()),
        hypervisor,
        container_hint,
        microcode: field("microcode"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareResult {
    pub host: HostInfo,
    pub binary: PathBuf,
    pub binary_sha256: Option<String>,
    /// `None` = native execution; `Some(path)` = user-mode emulator used (NOT hardware).
    pub emulator: Option<PathBuf>,
    pub emulated: bool,
    pub batches: usize,
    pub iters_per_batch: usize,
    pub total_iters: u64,
    #[serde(with = "crate::litmus::counts_serde")]
    pub counts: BTreeMap<Outcome, u64>,
    pub outcomes: Option<OutcomeSet>,
    pub abnormal_exits: usize,
    pub timeouts: usize,
    pub warnings: Vec<String>,
    pub command: Vec<String>,
}

/// Guess the cross sysroot for a `qemu-<arch>` binary from the Debian multiarch layout.
fn emulator_sysroot(emulator: &Path) -> Option<String> {
    let name = emulator.file_name()?.to_str()?;
    let triple = match name {
        "qemu-aarch64" => "aarch64-linux-gnu",
        "qemu-riscv64" => "riscv64-linux-gnu",
        "qemu-ppc64le" => "powerpc64le-linux-gnu",
        _ => return None,
    };
    let p = format!("/usr/{triple}");
    Path::new(&p).is_dir().then_some(p)
}

pub fn run_binary(binary: &Path, emulator: Option<&Path>, batches: usize, iters_per_batch: usize, timeout: Duration) -> HardwareResult {
    let binary_sha256 = std::fs::read(binary).ok().map(|b| {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(b))
    });
    let mut counts: BTreeMap<Outcome, u64> = BTreeMap::new();
    let mut abnormal = 0;
    let mut timeouts = 0;
    let mut warnings = Vec::new();
    let iters_arg = iters_per_batch.to_string();
    let spec = match emulator {
        Some(e) => {
            // qemu-user needs the target sysroot for the dynamic loader.
            let mut args: Vec<String> = Vec::new();
            if let Some(sysroot) = emulator_sysroot(e) {
                args.push("-L".into());
                args.push(sysroot);
            }
            args.push(binary.display().to_string());
            args.push(iters_arg.clone());
            RunSpec::new(e, args)
        }
        None => RunSpec::new(binary, [iters_arg.as_str()]),
    }
    .timeout(timeout);
    let command = spec.command_line();
    let mut total = 0u64;
    for b in 0..batches {
        match run(&spec) {
            Ok(o) if o.exit_code == Some(0) && !o.timed_out => match parse_histogram(&o.stdout) {
                Ok(h) => {
                    for (k, v) in h {
                        total += v;
                        *counts.entry(k).or_insert(0) += v;
                    }
                }
                Err(e) => {
                    abnormal += 1;
                    warnings.push(format!("batch {b}: unparsable output: {e}"));
                }
            },
            Ok(o) if o.timed_out => {
                timeouts += 1;
                warnings.push(format!("batch {b}: timed out after {timeout:?}"));
            }
            Ok(o) => {
                abnormal += 1;
                warnings.push(format!("batch {b}: exit {:?}: {}", o.exit_code, o.stderr.lines().last().unwrap_or("").trim()));
            }
            Err(e) => {
                abnormal += 1;
                warnings.push(format!("batch {b}: {e}"));
            }
        }
    }
    HardwareResult {
        host: host_info(),
        binary: binary.to_path_buf(),
        binary_sha256,
        emulator: emulator.map(Path::to_path_buf),
        emulated: emulator.is_some(),
        batches,
        iters_per_batch,
        total_iters: total,
        outcomes: (total > 0).then(|| OutcomeSet::from_counts(&counts, false)),
        counts,
        abnormal_exits: abnormal,
        timeouts,
        warnings,
        command,
    }
}

/// Wilson score interval for a proportion `k/n` at ~95% (z = 1.96).
pub fn wilson(k: u64, n: u64) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let z = 1.96f64;
    let n_f = n as f64;
    let p = k as f64 / n_f;
    let denom = 1.0 + z * z / n_f;
    let centre = p + z * z / (2.0 * n_f);
    let half = z * ((p * (1.0 - p) + z * z / (4.0 * n_f)) / n_f).sqrt();
    ((centre - half) / denom, (centre + half) / denom)
}

/// Upper 95% bound on the rate of an outcome that was never observed in `n` trials
/// (rule of three, exact form `1 - 0.05^(1/n)`).
pub fn zero_observation_upper_bound(n: u64) -> f64 {
    if n == 0 {
        return 1.0;
    }
    1.0 - 0.05f64.powf(1.0 / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_bounds_are_sane() {
        let (lo, hi) = wilson(50, 100);
        assert!(lo < 0.5 && hi > 0.5 && lo > 0.39 && hi < 0.61);
        let (lo, hi) = wilson(0, 1000);
        assert_eq!(lo, 0.0);
        assert!(hi < 0.005);
    }

    #[test]
    fn zero_bound() {
        assert!((zero_observation_upper_bound(1_000_000) - 3e-6).abs() < 1e-6);
        assert_eq!(zero_observation_upper_bound(0), 1.0);
    }

    #[test]
    fn host_info_populates_arch() {
        let h = host_info();
        assert!(!h.arch.is_empty());
    }
}
