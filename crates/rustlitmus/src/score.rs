//! Interest scoring for findings.
//!
//! A finding is one bundle's localisation result. The score is deliberately a small,
//! explicit weighted sum over *evidence signals* so that ranking is auditable. It ranks
//! candidates for follow-up (replication, reduction, prior-art search); it is not a
//! verdict.

use crate::evidence::{Bundle, Classification, Layer};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub name: String,
    pub value: f64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub total: f64,
    pub signals: Vec<Signal>,
    /// Coarse triage bucket derived from the dominant signal.
    pub bucket: Bucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    /// Hardware or an exhaustive later layer exhibits something the source model forbids.
    CandidateDefect,
    /// Two *source-level* oracles disagree (model vs emulator vs model checker).
    OracleDisagreement,
    /// The compiled program is provably stronger than the source model (expected).
    MappingStronger,
    /// The compiled program admits outcomes the source model forbids, but a documented
    /// weaker variant of the source model admits them (e.g. OOTA prohibition vs load buffering).
    KnownModelGap,
    /// Everything agreed.
    Consistent,
    /// Something could not be evaluated.
    Incomplete,
}

fn is_source(l: Layer) -> bool {
    matches!(
        l,
        Layer::SourceModel | Layer::SourceEmulator | Layer::SourceModelChecker
    )
}

pub fn score(b: &Bundle) -> Score {
    let mut signals = Vec::new();
    let mut bucket = Bucket::Consistent;

    // 1. Observed outside prediction (strongest): weight by which prediction was violated.
    for c in b
        .localization
        .adjacent
        .iter()
        .chain(b.localization.against_hardware.iter())
    {
        if let Classification::ObservedOutsidePrediction {
            layer_pred,
            layer_obs,
            outcomes,
        } = &c.classification
        {
            let w = match (layer_pred, layer_obs) {
                // Hardware contradicting the architecture model on *its own* compiled code:
                // either the model, the lifter, or the silicon is wrong. Very high.
                (Layer::ArchModel, Layer::Hardware) => 10.0,
                // Hardware contradicting the source model: compiler-mapping bug candidate
                // unless the arch model also allows it (then it's a mapping-weakness finding).
                (Layer::SourceModel | Layer::SourceModelChecker, Layer::Hardware) => 8.0,
                // Emulator producing something the exhaustive source model forbids: emulator
                // unsoundness w.r.t. that model (miri#5104 class) or model-version mismatch.
                (Layer::SourceModel | Layer::SourceModelChecker, Layer::SourceEmulator) => 5.0,
                _ => 3.0,
            };
            signals.push(Signal {
                name: "observed_outside_prediction".into(),
                value: w * outcomes.len() as f64,
                note: format!(
                    "{} observed {} outcome(s) forbidden by {}",
                    layer_obs.name(),
                    outcomes.len(),
                    layer_pred.name()
                ),
            });
            bucket = if matches!(layer_obs, Layer::Hardware) {
                Bucket::CandidateDefect
            } else if bucket != Bucket::CandidateDefect {
                Bucket::OracleDisagreement
            } else {
                bucket
            };
        }
    }
    // 2. Later exhaustive layer weaker than an earlier exhaustive layer.
    for c in &b.localization.adjacent {
        if let Classification::LaterLayerWeaker {
            earlier,
            later,
            outcomes,
        } = &c.classification
        {
            let w = if is_source(*earlier) && !is_source(*later) {
                7.0
            } else if is_source(*earlier) && is_source(*later) {
                4.0
            } else {
                3.0
            };
            signals.push(Signal {
                name: "later_layer_weaker".into(),
                value: w * outcomes.len() as f64,
                note: format!(
                    "{} allows {} outcome(s) {} forbids",
                    later.name(),
                    outcomes.len(),
                    earlier.name()
                ),
            });
            if bucket == Bucket::Consistent || bucket == Bucket::MappingStronger {
                bucket = if is_source(*later) {
                    Bucket::OracleDisagreement
                } else {
                    Bucket::CandidateDefect
                };
            }
        }
        if let Classification::LaterLayerStronger { .. } = &c.classification {
            signals.push(Signal { name: "mapping_stronger".into(), value: 0.2, note: "compiled program forbids outcomes the source model allows (expected for sound mappings)".into() });
            if bucket == Bucket::Consistent {
                bucket = Bucket::MappingStronger;
            }
        }
        if let Classification::ExplainedByModelGap {
            axiom, outcomes, ..
        } = &c.classification
        {
            signals.push(Signal {
                name: "explained_by_model_gap".into(),
                value: 0.5,
                note: format!(
                    "{} outcome(s) admitted only when `{axiom}` is dropped from the source model",
                    outcomes.len()
                ),
            });
            if bucket == Bucket::Consistent || bucket == Bucket::MappingStronger {
                bucket = Bucket::KnownModelGap;
            }
        }
        if let Classification::SampleCoverageDifference { a, b, .. } = &c.classification {
            signals.push(Signal {
                name: "sample_coverage_difference".into(),
                value: -0.5,
                note: format!(
                    "{} and {} produced different sampled outcome sets; increase coverage before drawing a conclusion",
                    a.name(),
                    b.name()
                ),
            });
            if bucket == Bucket::Consistent || bucket == Bucket::MappingStronger {
                bucket = Bucket::Incomplete;
            }
        }
        if let Classification::NotComparable { reason } = &c.classification {
            signals.push(Signal {
                name: "incomplete".into(),
                value: -0.5,
                note: reason.clone(),
            });
            if bucket == Bucket::Consistent {
                bucket = Bucket::Incomplete;
            }
        }
    }
    // 3. Independence: a single layer cannot establish cross-layer agreement.
    let n = b.layers.iter().filter(|l| l.outcomes.is_some()).count();
    if n < 2 && bucket == Bucket::Consistent {
        signals.push(Signal {
            name: "incomplete".into(),
            value: -0.5,
            note: format!("only {n} layer(s) produced an outcome set"),
        });
        bucket = Bucket::Incomplete;
    }
    signals.push(Signal {
        name: "independent_layers".into(),
        value: 0.3 * n as f64,
        note: format!("{n} layers produced outcome sets"),
    });
    // 4. Compiler-pipeline event change (atomic event shapes differ between IR layers).
    for tp in &b.pipeline {
        if let Some((a, c)) = tp
            .first_event_change
            .as_ref()
            .or(tp.first_ordering_change.as_ref())
        {
            let (signal_name, subject) = if tp.first_event_change.is_some() {
                ("pipeline_event_change", "atomic event shapes")
            } else {
                ("pipeline_ordering_change", "ordering annotations")
            };
            signals.push(Signal {
                name: signal_name.into(),
                value: 2.0,
                note: format!("{}: {subject} differ between {a} and {c}", tp.symbol),
            });
        }
    }
    // 5. Lift failure means the arch layer is missing: penalise slightly (unknown, not bad).
    if b.lift_error.is_some() {
        signals.push(Signal {
            name: "lift_unsupported".into(),
            value: -0.5,
            note: "assembly could not be lifted; arch-model layer missing".into(),
        });
    }
    let total = signals.iter().map(|s| s.value).sum();
    Score {
        total,
        signals,
        bucket,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{Comparison, Localization};

    fn loc(adjacent: Vec<Comparison>) -> Localization {
        Localization {
            chain: vec![],
            adjacent,
            against_hardware: vec![],
            earliest_divergence: None,
            summary: String::new(),
        }
    }

    #[test]
    fn buckets() {
        let mut b = crate::evidence::Bundle {
            schema_version: 1,
            case_id: "t".into(),
            litmus: crate::families::Family::SB.instance_from_seed(1),
            litmus_digest: String::new(),
            provenance: crate::evidence::Provenance {
                generator: "t".into(),
                generation_reason: "t".into(),
                seed: None,
                family: "SB".into(),
                parent_case: None,
                created_utc: String::new(),
            },
            rust_source: String::new(),
            rust_source_sha256: String::new(),
            c11_litmus: String::new(),
            compile: None,
            pipeline: vec![],
            lifted: None,
            lift_error: None,
            herd_source: None,
            herd_source_weak: None,
            herd_arch: None,
            miri_weak: None,
            miri_genmc: None,
            hardware: None,
            layers: vec![],
            localization: loc(vec![]),
            limitations: vec![],
            redactions: vec![],
            replay: vec![],
        };
        assert_eq!(score(&b).bucket, Bucket::Incomplete);
        b.localization = loc(vec![Comparison {
            a: Layer::SourceModel,
            b: Layer::ArchModel,
            classification: Classification::LaterLayerStronger {
                earlier: Layer::SourceModel,
                later: Layer::ArchModel,
                outcomes: vec![],
            },
        }]);
        assert_eq!(score(&b).bucket, Bucket::MappingStronger);
        b.localization = loc(vec![Comparison {
            a: Layer::SourceModel,
            b: Layer::ArchModel,
            classification: Classification::LaterLayerWeaker {
                earlier: Layer::SourceModel,
                later: Layer::ArchModel,
                outcomes: vec![crate::litmus::Outcome(vec![])],
            },
        }]);
        let s = score(&b);
        assert_eq!(s.bucket, Bucket::CandidateDefect);
        assert!(s.total > 5.0);
        b.localization = loc(vec![Comparison {
            a: Layer::SourceModel,
            b: Layer::SourceEmulator,
            classification: Classification::ObservedOutsidePrediction {
                layer_pred: Layer::SourceModel,
                layer_obs: Layer::SourceEmulator,
                outcomes: vec![crate::litmus::Outcome(vec![])],
            },
        }]);
        assert_eq!(score(&b).bucket, Bucket::OracleDisagreement);
        b.localization = loc(vec![Comparison {
            a: Layer::SourceEmulator,
            b: Layer::Hardware,
            classification: Classification::SampleCoverageDifference {
                a: Layer::SourceEmulator,
                b: Layer::Hardware,
                only_a: vec![crate::litmus::Outcome(vec![])],
                only_b: vec![],
            },
        }]);
        assert_eq!(score(&b).bucket, Bucket::Incomplete);
    }
}
