//! Adapter for `herd7` (herdtools7): run a litmus file under a `.cat` model and
//! parse the enumerated final states.

use crate::litmus::{Litmus, Outcome, OutcomeSet};
use crate::process::{run, RunSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HerdResult {
    pub tool: String,
    pub version: String,
    pub model: String,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Parsed states, keyed as printed by herd (`0:r0=1; 1:r0=0;`).
    pub raw_states: Vec<String>,
    pub outcomes: Option<OutcomeSet>,
    pub warnings: Vec<String>,
    /// Set when herd reported the model's `undefined_unless` clause (e.g. data race).
    pub undefined: bool,
}

pub fn herd_version(herd: &Path) -> String {
    match run(&RunSpec::new(herd, ["-version"]).timeout(Duration::from_secs(20))) {
        Ok(o) => o.stdout.lines().next().unwrap_or("").trim().to_string(),
        Err(e) => format!("unavailable: {e}"),
    }
}

/// Parse a herd7 final state line like `0:r0=1; 1:r1=0; 1:r0=2;` into an [`Outcome`]
/// with register counts taken from `litmus`. Registers absent in the line (herd omits
/// nothing when `locations` lists them, but be defensive) are an error.
pub fn parse_state_line(line: &str, litmus: &Litmus) -> Result<Outcome, String> {
    let mut regs: Vec<Vec<Option<u32>>> = litmus.threads.iter().map(|t| vec![None; t.num_regs()]).collect();
    for item in line.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (lhs, rhs) = item.split_once('=').ok_or_else(|| format!("bad state item {item:?}"))?;
        let (t, r) = lhs.split_once(":r").ok_or_else(|| format!("bad register {lhs:?}"))?;
        let t: usize = t.parse().map_err(|_| format!("bad thread in {lhs:?}"))?;
        let r: usize = r.parse().map_err(|_| format!("bad register in {lhs:?}"))?;
        let v: u32 = rhs.trim().parse().map_err(|_| format!("bad value in {item:?}"))?;
        let slot = regs.get_mut(t).and_then(|rs| rs.get_mut(r)).ok_or_else(|| format!("{lhs} out of range"))?;
        *slot = Some(v);
    }
    let mut out = Vec::new();
    for (t, rs) in regs.into_iter().enumerate() {
        let mut vals = Vec::new();
        for (r, v) in rs.into_iter().enumerate() {
            vals.push(v.ok_or_else(|| format!("register {t}:r{r} missing from state {line:?}"))?);
        }
        out.push(vals);
    }
    Ok(Outcome(out))
}

/// `decode` maps a herd state line to a source-level outcome; `None` records raw states
/// only (used when the caller decodes with its own register map).
pub type Decoder<'a> = &'a dyn Fn(&str) -> Result<Outcome, String>;

pub fn parse_output(stdout: &str, stderr: &str, decode: Option<Decoder<'_>>) -> (Vec<String>, Result<OutcomeSet, String>, Vec<String>, bool) {
    let mut raw = Vec::new();
    let mut outcomes = BTreeMap::new();
    let mut warnings: Vec<String> = stderr.lines().filter(|l| l.contains("Warning") || l.contains("error")).map(String::from).collect();
    let mut in_states = false;
    let mut states_declared: Option<usize> = None;
    let mut undefined = false;
    for line in stdout.lines() {
        if let Some(n) = line.strip_prefix("States ") {
            states_declared = n.trim().parse().ok();
            in_states = true;
            continue;
        }
        if in_states {
            let l = line.trim();
            if l.is_empty() || l == "Ok" || l == "No" || l.starts_with("Witnesses") {
                in_states = false;
                continue;
            }
            raw.push(l.to_string());
            if let Some(d) = decode {
                match d(l) {
                    Ok(o) => {
                        *outcomes.entry(o).or_insert(0u64) += 1;
                    }
                    Err(e) => warnings.push(format!("unparsed state {l:?}: {e}")),
                }
            }
        }
        if line.contains("Undefined") || line.contains("undefined") {
            undefined = true;
        }
    }
    let parsed = match states_declared {
        None => Err(format!("herd output contained no `States` section; stderr: {}", stderr.trim())),
        Some(n) if n != raw.len() => Err(format!("herd declared {n} states but printed {}", raw.len())),
        Some(_) if decode.is_some() && outcomes.values().sum::<u64>() != raw.len() as u64 => Err("some herd states could not be decoded".into()),
        Some(_) if decode.is_none() => Err("no decoder supplied for herd states".into()),
        Some(_) => Ok(OutcomeSet::from_counts(&outcomes, true)),
    };
    (raw, parsed, warnings, undefined)
}

pub fn run_herd(herd: &Path, model: &str, litmus_file: &Path, decode: Option<Decoder<'_>>, timeout: Duration) -> HerdResult {
    let args: Vec<String> = vec!["-model".into(), model.into(), "-show".into(), "none".into(), litmus_file.display().to_string()];
    let spec = RunSpec::new(herd, args.iter().map(String::as_str)).timeout(timeout);
    let mut command = vec![herd.display().to_string()];
    command.extend(args.iter().cloned());
    let version = herd_version(herd);
    match run(&spec) {
        Ok(o) => {
            let (raw_states, parsed, warnings, undefined) = parse_output(&o.stdout, &o.stderr, decode);
            let (outcomes, warnings) = match parsed {
                Ok(set) => (Some(set), warnings),
                Err(e) => {
                    let mut w = warnings;
                    w.push(e);
                    (None, w)
                }
            };
            HerdResult { tool: "herd7".into(), version, model: model.into(), command, exit_code: o.exit_code, stdout: o.stdout, stderr: o.stderr, raw_states, outcomes, warnings, undefined }
        }
        Err(e) => HerdResult {
            tool: "herd7".into(),
            version,
            model: model.into(),
            command,
            exit_code: None,
            stdout: String::new(),
            stderr: e.to_string(),
            raw_states: vec![],
            outcomes: None,
            warnings: vec![format!("herd7 failed to run: {e}")],
            undefined: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litmus::{Instr, Ord, Thread};

    fn mp() -> Litmus {
        Litmus {
            name: "MP".into(),
            locations: vec!["x".into(), "y".into()],
            threads: vec![
                Thread {
                    instrs: vec![
                        Instr::Store { loc: 0, value: 1, ord: Ord::Relaxed },
                        Instr::Store { loc: 1, value: 1, ord: Ord::Release },
                    ],
                },
                Thread {
                    instrs: vec![
                        Instr::Load { loc: 1, reg: 0, ord: Ord::Acquire },
                        Instr::Load { loc: 0, reg: 1, ord: Ord::Relaxed },
                    ],
                },
            ],
        }
    }

    #[test]
    fn parses_herd_states() {
        let out = "Test MP Allowed\nStates 3\n1:r0=0; 1:r1=0;\n1:r0=0; 1:r1=1;\n1:r0=1; 1:r1=1;\nOk\nWitnesses\nPositive: 3 Negative: 0\n";
        let m = mp();
        let dec = |l: &str| parse_state_line(l, &m);
        let (raw, parsed, warnings, undef) = parse_output(out, "", Some(&dec));
        assert_eq!(raw.len(), 3);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!undef);
        let set = parsed.unwrap();
        assert!(set.exhaustive);
        assert_eq!(set.outcomes.len(), 3);
        assert!(set.contains(&Outcome(vec![vec![], vec![1, 1]])));
        assert!(!set.contains(&Outcome(vec![vec![], vec![1, 0]])));
    }

    #[test]
    fn detects_state_count_mismatch_and_missing_section() {
        let out = "Test MP Allowed\nStates 2\n1:r0=0; 1:r1=0;\nOk\n";
        let m = mp();
        let dec = |l: &str| parse_state_line(l, &m);
        let (_, parsed, _, _) = parse_output(out, "", Some(&dec));
        assert!(parsed.is_err());
        let (_, parsed, _, _) = parse_output("garbage", "Warning: boom", Some(&dec));
        assert!(parsed.is_err());
        // Undecodable states are an error, not silently dropped.
        let bad = "Test MP Allowed\nStates 1\n1:r0=0;\nOk\n";
        let (_, parsed, warnings, _) = parse_output(bad, "", Some(&dec));
        assert!(parsed.is_err());
        assert!(!warnings.is_empty());
    }

    #[test]
    fn rejects_missing_register() {
        assert!(parse_state_line("1:r0=0;", &mp()).is_err());
        assert!(parse_state_line("1:r0=0; 1:r1=zz;", &mp()).is_err());
        assert!(parse_state_line("7:r0=0; 1:r1=0;", &mp()).is_err());
    }
}
