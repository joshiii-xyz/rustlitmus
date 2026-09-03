//! Abstract litmus-test model.
//!
//! A [`Litmus`] is a small, loop-free concurrent program over a fixed set of
//! shared atomic locations. It is the *single source of truth* from which the
//! Rust program, the C11 program for `herd7`, and the lifted architecture-level
//! test are all derived, so that every layer is comparing the same program.
//!
//! The model is deliberately restricted to the litmus-test shape that
//! `herd7`'s language-level models (`rc11.cat`, `c11_*.cat`) can simulate:
//! straight-line threads, atomic loads/stores/RMWs/fences, and outcomes
//! expressed over the final register values of loads.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Memory orderings available on stable Rust atomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ord {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

impl Ord {
    pub const ALL: [Ord; 5] = [
        Ord::Relaxed,
        Ord::Acquire,
        Ord::Release,
        Ord::AcqRel,
        Ord::SeqCst,
    ];

    pub fn rust(self) -> &'static str {
        match self {
            Ord::Relaxed => "Relaxed",
            Ord::Acquire => "Acquire",
            Ord::Release => "Release",
            Ord::AcqRel => "AcqRel",
            Ord::SeqCst => "SeqCst",
        }
    }

    pub fn c11(self) -> &'static str {
        match self {
            Ord::Relaxed => "memory_order_relaxed",
            Ord::Acquire => "memory_order_acquire",
            Ord::Release => "memory_order_release",
            Ord::AcqRel => "memory_order_acq_rel",
            Ord::SeqCst => "memory_order_seq_cst",
        }
    }

    /// Orderings a plain load may carry (Rust panics on Release/AcqRel).
    pub fn valid_for_load(self) -> bool {
        matches!(self, Ord::Relaxed | Ord::Acquire | Ord::SeqCst)
    }

    /// Orderings a plain store may carry (Rust panics on Acquire/AcqRel).
    pub fn valid_for_store(self) -> bool {
        matches!(self, Ord::Relaxed | Ord::Release | Ord::SeqCst)
    }

    /// Orderings a `compare_exchange` failure may carry (Rust panics on Release/AcqRel).
    pub fn valid_for_cas_failure(self) -> bool {
        matches!(self, Ord::Relaxed | Ord::Acquire | Ord::SeqCst)
    }

    /// Orderings a fence may carry (Rust panics on Relaxed).
    pub fn valid_for_fence(self) -> bool {
        !matches!(self, Ord::Relaxed)
    }

    pub fn short(self) -> &'static str {
        match self {
            Ord::Relaxed => "rlx",
            Ord::Acquire => "acq",
            Ord::Release => "rel",
            Ord::AcqRel => "ar",
            Ord::SeqCst => "sc",
        }
    }
}

/// Read-modify-write flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RmwKind {
    /// `swap(v)` / `atomic_exchange`.
    Swap,
    /// `fetch_add(v)` / `atomic_fetch_add`.
    FetchAdd,
    /// `compare_exchange(expected, v, success, failure)`; the register receives the
    /// *previous* value (Rust: `Ok(prev)` or `Err(prev)`, both flattened to `prev`).
    CompareExchange { expected: u32, failure: Ord },
}

/// One instruction in a thread. Locations are indices into [`Litmus::locations`];
/// registers are per-thread indices into the outcome tuple.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Instr {
    Store {
        loc: usize,
        value: u32,
        ord: Ord,
    },
    Load {
        loc: usize,
        reg: usize,
        ord: Ord,
    },
    Rmw {
        loc: usize,
        reg: usize,
        value: u32,
        ord: Ord,
        kind: RmwKind,
    },
    Fence {
        ord: Ord,
    },
}

impl Instr {
    pub fn loc(&self) -> Option<usize> {
        match self {
            Instr::Store { loc, .. } | Instr::Load { loc, .. } | Instr::Rmw { loc, .. } => {
                Some(*loc)
            }
            Instr::Fence { .. } => None,
        }
    }
    pub fn reg(&self) -> Option<usize> {
        match self {
            Instr::Load { reg, .. } | Instr::Rmw { reg, .. } => Some(*reg),
            _ => None,
        }
    }
    pub fn is_write(&self) -> bool {
        matches!(self, Instr::Store { .. } | Instr::Rmw { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Thread {
    pub instrs: Vec<Instr>,
}

impl Thread {
    pub fn num_regs(&self) -> usize {
        self.instrs
            .iter()
            .filter_map(Instr::reg)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0)
    }
}

/// A complete litmus test.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Litmus {
    pub name: String,
    /// Shared atomic `u32` locations, all initialised to `0`.
    pub locations: Vec<String>,
    pub threads: Vec<Thread>,
}

/// One complete final state: for each thread, its register values in order.
/// Registers never written by a load are absent (the thread vector is exactly
/// `num_regs()` long).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Outcome(pub Vec<Vec<u32>>);

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (t, regs) in self.0.iter().enumerate() {
            for (r, v) in regs.iter().enumerate() {
                if !first {
                    write!(f, " ")?;
                }
                first = false;
                write!(f, "{t}:r{r}={v}")?;
            }
        }
        if first {
            write!(f, "(no registers)")?;
        }
        Ok(())
    }
}

/// A set of outcomes together with the *epistemic status* of the set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeSet {
    pub outcomes: Vec<Outcome>,
    /// `true` when the producer claims the set is complete (an axiomatic model or
    /// an exhaustive model checker); `false` for finite sampling (Miri seeds,
    /// hardware).
    pub exhaustive: bool,
}

impl OutcomeSet {
    pub fn from_counts(counts: &BTreeMap<Outcome, u64>, exhaustive: bool) -> Self {
        OutcomeSet {
            outcomes: counts.keys().cloned().collect(),
            exhaustive,
        }
    }
    pub fn contains(&self, o: &Outcome) -> bool {
        self.outcomes.contains(o)
    }
}

/// Serde adapter: JSON objects need string keys, so outcome histograms are stored as a
/// list of `{"outcome": [[..],[..]], "count": n}` records (sorted by outcome).
pub mod counts_serde {
    use super::Outcome;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize)]
    struct Rec {
        outcome: Outcome,
        count: u64,
    }

    pub fn serialize<S: Serializer>(m: &BTreeMap<Outcome, u64>, s: S) -> Result<S::Ok, S::Error> {
        let v: Vec<Rec> = m
            .iter()
            .map(|(o, c)| Rec {
                outcome: o.clone(),
                count: *c,
            })
            .collect();
        v.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<Outcome, u64>, D::Error> {
        let v: Vec<Rec> = Vec::deserialize(d)?;
        Ok(v.into_iter().map(|r| (r.outcome, r.count)).collect())
    }
}

impl Litmus {
    pub fn validate(&self) -> Result<(), String> {
        if self.threads.is_empty() {
            return Err("litmus has no threads".into());
        }
        if self.locations.is_empty() {
            return Err("litmus has no locations".into());
        }
        for (t, th) in self.threads.iter().enumerate() {
            let mut seen_regs = vec![false; th.num_regs()];
            for (i, ins) in th.instrs.iter().enumerate() {
                if let Some(l) = ins.loc() {
                    if l >= self.locations.len() {
                        return Err(format!("thread {t} instr {i}: location {l} out of range"));
                    }
                }
                if let Some(r) = ins.reg() {
                    if seen_regs[r] {
                        return Err(format!("thread {t} instr {i}: register r{r} written twice"));
                    }
                    seen_regs[r] = true;
                }
                match ins {
                    Instr::Store { ord, .. } if !ord.valid_for_store() => {
                        return Err(format!(
                            "thread {t} instr {i}: invalid store ordering {ord:?}"
                        ))
                    }
                    Instr::Load { ord, .. } if !ord.valid_for_load() => {
                        return Err(format!(
                            "thread {t} instr {i}: invalid load ordering {ord:?}"
                        ))
                    }
                    Instr::Fence { ord } if !ord.valid_for_fence() => {
                        return Err(format!(
                            "thread {t} instr {i}: invalid fence ordering {ord:?}"
                        ))
                    }
                    Instr::Rmw {
                        kind: RmwKind::CompareExchange { failure, .. },
                        ..
                    } if !failure.valid_for_cas_failure() => {
                        return Err(format!(
                            "thread {t} instr {i}: invalid CAS failure ordering {failure:?}"
                        ))
                    }
                    _ => {}
                }
            }
            if seen_regs.iter().any(|s| !s) {
                return Err(format!("thread {t}: registers must be dense (r0..rN)"));
            }
        }
        Ok(())
    }

    /// All values that can ever be stored to any location (used to bound outcome
    /// enumeration and to name values in lifted tests).
    pub fn written_values(&self) -> Vec<u32> {
        let mut v: Vec<u32> = vec![0];
        for th in &self.threads {
            for ins in &th.instrs {
                match ins {
                    Instr::Store { value, .. } | Instr::Rmw { value, .. } => v.push(*value),
                    _ => {}
                }
            }
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Stable content digest of the *abstract* program (not of any rendering).
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let json = serde_json::to_vec(self).expect("litmus serialises");
        hex::encode(Sha256::digest(json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sb() -> Litmus {
        Litmus {
            name: "SB".into(),
            locations: vec!["x".into(), "y".into()],
            threads: vec![
                Thread {
                    instrs: vec![
                        Instr::Store {
                            loc: 0,
                            value: 1,
                            ord: Ord::Relaxed,
                        },
                        Instr::Load {
                            loc: 1,
                            reg: 0,
                            ord: Ord::Relaxed,
                        },
                    ],
                },
                Thread {
                    instrs: vec![
                        Instr::Store {
                            loc: 1,
                            value: 1,
                            ord: Ord::Relaxed,
                        },
                        Instr::Load {
                            loc: 0,
                            reg: 0,
                            ord: Ord::Relaxed,
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn validates_sb() {
        sb().validate().unwrap();
    }

    #[test]
    fn rejects_bad_orderings() {
        let mut l = sb();
        l.threads[0].instrs[0] = Instr::Store {
            loc: 0,
            value: 1,
            ord: Ord::Acquire,
        };
        assert!(l.validate().is_err());
        let mut l = sb();
        l.threads[0].instrs[1] = Instr::Load {
            loc: 1,
            reg: 0,
            ord: Ord::Release,
        };
        assert!(l.validate().is_err());
        let mut l = sb();
        l.threads[0].instrs.push(Instr::Fence { ord: Ord::Relaxed });
        assert!(l.validate().is_err());
    }

    #[test]
    fn rejects_sparse_registers() {
        let mut l = sb();
        l.threads[0].instrs[1] = Instr::Load {
            loc: 1,
            reg: 1,
            ord: Ord::Relaxed,
        };
        assert!(l.validate().is_err());
    }

    #[test]
    fn digest_is_stable_and_content_addressed() {
        let a = sb().digest();
        let b = sb().digest();
        assert_eq!(a, b);
        let mut l = sb();
        l.threads[0].instrs[0] = Instr::Store {
            loc: 0,
            value: 1,
            ord: Ord::SeqCst,
        };
        assert_ne!(a, l.digest());
    }

    #[test]
    fn outcome_display() {
        let o = Outcome(vec![vec![0], vec![1]]);
        assert_eq!(o.to_string(), "0:r0=0 1:r0=1");
    }
}
