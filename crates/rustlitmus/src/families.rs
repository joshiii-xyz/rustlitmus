//! Semantic litmus families and their ordering-parameterised enumeration.
//!
//! Each family is a well-known shape from the weak-memory literature. A *case* is a
//! family instance with concrete orderings for every access. Enumerating orderings
//! per family explores exactly the region where compiler mappings and memory models
//! interact; nothing here is random syntax.

use crate::litmus::{Instr, Litmus, Ord, RmwKind, Thread};

/// Family identifiers. Names follow the diy7/herd naming convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// Store buffering: W x; R y || W y; R x.  Weak outcome r0=r1=0.
    SB,
    /// Message passing: W x; W y || R y; R x.  Weak outcome r0=1,r1=0.
    MP,
    /// Load buffering: R x; W y || R y; W x.  Weak outcome r0=r1=1.
    LB,
    /// Independent reads of independent writes: W x || W y || R x; R y || R y; R x.
    IRIW,
    /// Write-to-read causality: W x || R x; W y || R y; R x.
    WRC,
    /// 2+2W: W x=1; W y=2 || W y=1; W x=2. Weak outcome x=2? (needs final reads; we add
    /// reader loads in each thread after a fence-free ordering).
    TwoPlusTwoW,
    /// Store buffering with the stores replaced by RMWs (exercises RMW mappings).
    SBRmw,
    /// MP where the release is an RMW (release sequence head as RMW).
    MPRmw,
    /// MP where a relaxed store by the *same* thread follows the release (C++20 P0982:
    /// same-thread relaxed store no longer continues the release sequence).
    MPReleaseSeq,
    /// MP through a CAS on the flag: the reader's acquire is the *failure* ordering of a
    /// compare_exchange that always fails (exercises the failure-ordering mapping).
    MPCasFail,
    /// Fenced SB: W x; F; R y || W y; F; R x.
    SBFence,
    /// Fenced MP: W x; F; W y || R y; F; R x.
    MPFence,
    /// Fenced LB: R x; F; W y || R y; F; W x.
    LBFence,
    /// R (a.k.a. "R" test): W x=1; W y=1 || W y=2; R x. Weak: y=2 /\ r=0.
    R,
    /// S: W x=2; W y=1 || R y; W x=1. Weak: x=2 /\ r=1.
    S,
}

impl Family {
    pub const ALL: [Family; 15] = [
        Family::SB,
        Family::MP,
        Family::LB,
        Family::IRIW,
        Family::WRC,
        Family::TwoPlusTwoW,
        Family::SBRmw,
        Family::MPRmw,
        Family::MPReleaseSeq,
        Family::MPCasFail,
        Family::SBFence,
        Family::MPFence,
        Family::LBFence,
        Family::R,
        Family::S,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Family::SB => "SB",
            Family::MP => "MP",
            Family::LB => "LB",
            Family::IRIW => "IRIW",
            Family::WRC => "WRC",
            Family::TwoPlusTwoW => "2+2W",
            Family::SBRmw => "SB+rmw",
            Family::MPRmw => "MP+rmw",
            Family::MPReleaseSeq => "MP+relseq",
            Family::MPCasFail => "MP+casfail",
            Family::SBFence => "SB+fence",
            Family::MPFence => "MP+fence",
            Family::LBFence => "LB+fence",
            Family::R => "R",
            Family::S => "S",
        }
    }

    pub fn from_name(s: &str) -> Option<Family> {
        Family::ALL
            .iter()
            .copied()
            .find(|f| f.name().eq_ignore_ascii_case(s))
    }

    /// Number of ordering slots this family exposes.
    pub fn slots(self) -> usize {
        match self {
            Family::SB | Family::MP | Family::LB | Family::SBRmw | Family::MPRmw => 4,
            Family::IRIW => 6,
            Family::WRC => 5,
            Family::TwoPlusTwoW => 6,
            Family::MPReleaseSeq => 5,
            Family::MPCasFail => 5,
            Family::SBFence | Family::MPFence | Family::LBFence => 6,
            Family::R => 5,
            Family::S => 5,
        }
    }

    /// Which orderings are legal in slot `i` (store slots cannot be Acquire, etc.).
    pub fn slot_domain(self, i: usize) -> &'static [Ord] {
        const ST: &[Ord] = &[Ord::Relaxed, Ord::Release, Ord::SeqCst];
        const LD: &[Ord] = &[Ord::Relaxed, Ord::Acquire, Ord::SeqCst];
        const RMW: &[Ord] = &[
            Ord::Relaxed,
            Ord::Acquire,
            Ord::Release,
            Ord::AcqRel,
            Ord::SeqCst,
        ];
        const FENCE: &[Ord] = &[Ord::Acquire, Ord::Release, Ord::AcqRel, Ord::SeqCst];
        match (self, i) {
            (Family::SB, 0 | 2) | (Family::MP, 0 | 1) | (Family::LB, 1 | 3) => ST,
            (Family::SB, _) | (Family::MP, _) | (Family::LB, _) => LD,
            (Family::IRIW, 0 | 1) => ST,
            (Family::IRIW, _) => LD,
            (Family::WRC, 0 | 2) => ST,
            (Family::WRC, _) => LD,
            (Family::TwoPlusTwoW, 0..=3) => ST,
            (Family::TwoPlusTwoW, _) => LD,
            (Family::SBRmw, 0 | 2) => RMW,
            (Family::SBRmw, _) => LD,
            (Family::MPRmw, 0) => ST,
            (Family::MPRmw, 1) => RMW,
            (Family::MPRmw, _) => LD,
            (Family::MPReleaseSeq, 0..=2) => ST,
            (Family::MPReleaseSeq, _) => LD,
            (Family::MPCasFail, 0 | 1) => ST,
            (Family::MPCasFail, 2) => RMW,
            (Family::MPCasFail, 3) => LD, // CAS failure ordering
            (Family::MPCasFail, _) => LD,
            (Family::SBFence, 0 | 3) => ST,
            (Family::SBFence, 1 | 4) => FENCE,
            (Family::SBFence, _) => LD,
            (Family::MPFence, 0 | 2) => ST,
            (Family::MPFence, 1 | 4) => FENCE,
            (Family::MPFence, _) => LD,
            (Family::LBFence, 0 | 3) => LD,
            (Family::LBFence, 1 | 4) => FENCE,
            (Family::LBFence, _) => ST,
            (Family::R, 0..=2) => ST,
            (Family::R, _) => LD,
            (Family::S, 0 | 1 | 3) => ST,
            (Family::S, _) => LD,
        }
    }

    /// Instantiate with concrete orderings (`ords.len() == slots()`).
    pub fn instantiate(self, ords: &[Ord]) -> Litmus {
        assert_eq!(
            ords.len(),
            self.slots(),
            "wrong number of orderings for {}",
            self.name()
        );
        let o = |i: usize| ords[i];
        let st = |loc, value, ord| Instr::Store { loc, value, ord };
        let ld = |loc, reg, ord| Instr::Load { loc, reg, ord };
        let xchg = |loc, reg, value, ord| Instr::Rmw {
            loc,
            reg,
            value,
            ord,
            kind: RmwKind::Swap,
        };
        let fence = |ord| Instr::Fence { ord };
        let xy = || vec!["x".to_string(), "y".to_string()];
        let name = format!(
            "{}+{}",
            self.name(),
            ords.iter().map(|o| o.short()).collect::<Vec<_>>().join("-")
        );
        let threads = match self {
            Family::SB => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), ld(1, 0, o(1))],
                },
                Thread {
                    instrs: vec![st(1, 1, o(2)), ld(0, 0, o(3))],
                },
            ],
            Family::MP => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), st(1, 1, o(1))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(2)), ld(0, 1, o(3))],
                },
            ],
            Family::LB => vec![
                Thread {
                    instrs: vec![ld(0, 0, o(0)), st(1, 1, o(1))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(2)), st(0, 1, o(3))],
                },
            ],
            Family::IRIW => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0))],
                },
                Thread {
                    instrs: vec![st(1, 1, o(1))],
                },
                Thread {
                    instrs: vec![ld(0, 0, o(2)), ld(1, 1, o(3))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(4)), ld(0, 1, o(5))],
                },
            ],
            Family::WRC => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0))],
                },
                Thread {
                    instrs: vec![ld(0, 0, o(1)), st(1, 1, o(2))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(3)), ld(0, 1, o(4))],
                },
            ],
            Family::TwoPlusTwoW => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), st(1, 2, o(1)), ld(0, 0, o(4))],
                },
                Thread {
                    instrs: vec![st(1, 1, o(2)), st(0, 2, o(3)), ld(1, 0, o(5))],
                },
            ],
            Family::SBRmw => vec![
                Thread {
                    instrs: vec![xchg(0, 0, 1, o(0)), ld(1, 1, o(1))],
                },
                Thread {
                    instrs: vec![xchg(1, 0, 1, o(2)), ld(0, 1, o(3))],
                },
            ],
            Family::MPRmw => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), xchg(1, 0, 1, o(1))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(2)), ld(0, 1, o(3))],
                },
            ],
            Family::MPReleaseSeq => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), st(1, 1, o(1)), st(1, 2, o(2))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(3)), ld(0, 1, o(4))],
                },
            ],
            Family::MPCasFail => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), st(1, 1, o(1))],
                },
                // CAS expects 7 (never present) so it always fails and acts as a load with the failure ordering.
                Thread {
                    instrs: vec![
                        Instr::Rmw {
                            loc: 1,
                            reg: 0,
                            value: 9,
                            ord: o(2),
                            kind: RmwKind::CompareExchange {
                                expected: 7,
                                failure: o(3),
                            },
                        },
                        ld(0, 1, o(4)),
                    ],
                },
            ],
            Family::SBFence => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), fence(o(1)), ld(1, 0, o(2))],
                },
                Thread {
                    instrs: vec![st(1, 1, o(3)), fence(o(4)), ld(0, 0, o(5))],
                },
            ],
            Family::MPFence => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), fence(o(1)), st(1, 1, o(2))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(3)), fence(o(4)), ld(0, 1, o(5))],
                },
            ],
            Family::LBFence => vec![
                Thread {
                    instrs: vec![ld(0, 0, o(0)), fence(o(1)), st(1, 1, o(2))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(3)), fence(o(4)), st(0, 1, o(5))],
                },
            ],
            Family::R => vec![
                Thread {
                    instrs: vec![st(0, 1, o(0)), st(1, 1, o(1))],
                },
                Thread {
                    instrs: vec![st(1, 2, o(2)), ld(0, 0, o(3)), ld(1, 1, o(4))],
                },
            ],
            Family::S => vec![
                Thread {
                    instrs: vec![st(0, 2, o(0)), st(1, 1, o(1))],
                },
                Thread {
                    instrs: vec![ld(1, 0, o(2)), st(0, 1, o(3)), ld(0, 1, o(4))],
                },
            ],
        };
        Litmus {
            name,
            locations: xy(),
            threads,
        }
    }

    /// Enumerate every legal ordering assignment (cartesian product of slot domains).
    pub fn all_instances(self) -> Vec<Litmus> {
        let mut out = Vec::new();
        let n = self.slots();
        let mut idx = vec![0usize; n];
        loop {
            let ords: Vec<Ord> = (0..n).map(|i| self.slot_domain(i)[idx[i]]).collect();
            out.push(self.instantiate(&ords));
            // increment
            let mut i = 0;
            loop {
                if i == n {
                    return out;
                }
                idx[i] += 1;
                if idx[i] < self.slot_domain(i).len() {
                    break;
                }
                idx[i] = 0;
                i += 1;
            }
        }
    }

    /// Deterministic pseudo-random instance from a seed (for sampled campaigns).
    pub fn instance_from_seed(self, seed: u64) -> Litmus {
        let mut s = seed ^ 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let ords: Vec<Ord> = (0..self.slots())
            .map(|i| {
                let d = self.slot_domain(i);
                d[(next() % d.len() as u64) as usize]
            })
            .collect();
        self.instantiate(&ords)
    }
}

/// Parse `FAMILY+o1-o2-...` back into a litmus (inverse of `instantiate` naming).
pub fn parse_case_name(name: &str) -> Option<Litmus> {
    let (fam, ords) = name
        .split_once('+')
        .map(|(f, o)| (f, Some(o)))
        .unwrap_or((name, None));
    // Families with '+' in their own name (e.g. MP+rmw) need a second split attempt.
    for f in Family::ALL {
        if let Some(rest) = name
            .strip_prefix(f.name())
            .and_then(|r| r.strip_prefix('+'))
        {
            let ords: Option<Vec<Ord>> = rest.split('-').map(short_to_ord).collect();
            if let Some(ords) = ords {
                if ords.len() == f.slots() {
                    return Some(f.instantiate(&ords));
                }
            }
        }
    }
    let _ = (fam, ords);
    None
}

fn short_to_ord(s: &str) -> Option<Ord> {
    Some(match s {
        "rlx" => Ord::Relaxed,
        "acq" => Ord::Acquire,
        "rel" => Ord::Release,
        "ar" => Ord::AcqRel,
        "sc" => Ord::SeqCst,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_instances_validate() {
        for f in Family::ALL {
            let all = f.all_instances();
            assert!(!all.is_empty());
            for l in &all {
                l.validate().unwrap_or_else(|e| panic!("{}: {e}", l.name));
            }
            // Names are unique within a family.
            let mut names: Vec<&str> = all.iter().map(|l| l.name.as_str()).collect();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), all.len(), "duplicate names in {}", f.name());
        }
    }

    #[test]
    fn instance_counts() {
        assert_eq!(Family::SB.all_instances().len(), 81);
        assert_eq!(Family::MPCasFail.all_instances().len(), 3 * 3 * 5 * 3 * 3);
    }

    #[test]
    fn round_trips_names() {
        for f in Family::ALL {
            let l = f.instance_from_seed(42);
            let back = parse_case_name(&l.name).unwrap();
            assert_eq!(back, l);
        }
        assert!(parse_case_name("bogus+rlx").is_none());
    }

    #[test]
    fn seeded_instances_are_deterministic() {
        assert_eq!(
            Family::IRIW.instance_from_seed(7),
            Family::IRIW.instance_from_seed(7)
        );
    }
}
