//! Evidence bundle: the single artifact that records everything the pipeline
//! established for one case under one configuration, with every value tagged by
//! epistemic status, plus the cross-layer comparison and its classification.

use crate::compile::{CompileConfig, CompileResult, Event, LayerEvents};
use crate::hardware::HardwareResult;
use crate::herd::HerdResult;
use crate::lift::{LiftError, Lifted};
use crate::litmus::{Litmus, Outcome, OutcomeSet};
use crate::miri::MiriResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const SCHEMA_VERSION: u32 = 1;

/// Epistemic status of a value in the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Observed,
    Predicted,
    Inferred,
    Assumed,
    Unknown,
    Unsupported,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tagged<T> {
    pub status: Status,
    pub value: T,
}

impl<T> Tagged<T> {
    pub fn observed(v: T) -> Self {
        Tagged {
            status: Status::Observed,
            value: v,
        }
    }
    pub fn predicted(v: T) -> Self {
        Tagged {
            status: Status::Predicted,
            value: v,
        }
    }
}

/// Which semantic layer produced an outcome set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Language-level axiomatic model (`herd7 rc11.cat` on the C11 rendering).
    SourceModel,
    /// Operational emulator of the language model (Miri weak memory, sampled).
    SourceEmulator,
    /// Exhaustive model checker at MIR level (Miri-GenMC).
    SourceModelChecker,
    /// Architecture-level model on the lifted compiled assembly (`herd7 aarch64.cat` / `x86tso.cat`).
    ArchModel,
    /// Native execution.
    Hardware,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Layer::SourceModel => "source-model(herd7/rc11)",
            Layer::SourceEmulator => "source-emulator(miri/weak-memory)",
            Layer::SourceModelChecker => "source-model-checker(miri/genmc)",
            Layer::ArchModel => "arch-model(herd7 on lifted asm)",
            Layer::Hardware => "hardware",
        }
    }
}

/// Rich classification of a comparison between two outcome sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Classification {
    /// Sets are equal (both exhaustive) or the sample is contained in the prediction.
    Consistent,
    /// A sampled/observed outcome is absent from an exhaustive prediction. This is the
    /// strongest signal: something observed that a model forbids.
    ObservedOutsidePrediction {
        layer_pred: Layer,
        layer_obs: Layer,
        outcomes: Vec<Outcome>,
    },
    /// An exhaustive layer permits outcomes a *later* exhaustive layer forbids: the
    /// compilation mapping is stronger than the source model at this point (expected and
    /// benign for correct mappings) — recorded, not flagged.
    LaterLayerStronger {
        earlier: Layer,
        later: Layer,
        outcomes: Vec<Outcome>,
    },
    /// An exhaustive later layer permits outcomes an exhaustive earlier layer forbids:
    /// the compiled program admits behaviour the source model does not. Compiler-mapping
    /// bug candidate *or* model limitation; requires adjudication.
    LaterLayerWeaker {
        earlier: Layer,
        later: Layer,
        outcomes: Vec<Outcome>,
    },
    /// Two sampled layers disagree; only informative about coverage.
    SampleCoverageDifference {
        a: Layer,
        b: Layer,
        only_a: Vec<Outcome>,
        only_b: Vec<Outcome>,
    },
    /// A later layer admits outcomes the primary source model forbids, but a documented
    /// weaker variant of the source model (with the named axiom removed) admits them.
    /// This is diagnostic compatibility only: it does not rule out a compiler-mapping
    /// problem without independent adjudication.
    ExplainedByModelGap {
        earlier: Layer,
        later: Layer,
        axiom: String,
        outcomes: Vec<Outcome>,
    },
    /// One side unavailable.
    NotComparable { reason: String },
}

/// Whether this comparison needs a human follow-up. Stronger compiled mappings are
/// expected for sound compilation, while unavailable layers and diagnostic model context
/// do not establish a candidate.
pub fn requires_follow_up(classification: &Classification) -> bool {
    matches!(
        classification,
        Classification::ObservedOutsidePrediction { .. }
            | Classification::LaterLayerWeaker { .. }
            | Classification::SampleCoverageDifference { .. }
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comparison {
    pub a: Layer,
    pub b: Layer,
    pub classification: Classification,
}

pub fn compare(a: Layer, sa: Option<&OutcomeSet>, b: Layer, sb: Option<&OutcomeSet>) -> Comparison {
    let (Some(sa), Some(sb)) = (sa, sb) else {
        let missing = if sa.is_none() { a } else { b };
        return Comparison {
            a,
            b,
            classification: Classification::NotComparable {
                reason: format!("{} produced no outcome set", missing.name()),
            },
        };
    };
    let set_a: BTreeSet<&Outcome> = sa.outcomes.iter().collect();
    let set_b: BTreeSet<&Outcome> = sb.outcomes.iter().collect();
    let only_a: Vec<Outcome> = set_a.difference(&set_b).map(|o| (*o).clone()).collect();
    let only_b: Vec<Outcome> = set_b.difference(&set_a).map(|o| (*o).clone()).collect();
    let cls = match (sa.exhaustive, sb.exhaustive) {
        (true, true) => {
            if only_a.is_empty() && only_b.is_empty() {
                Classification::Consistent
            } else if !only_b.is_empty() {
                Classification::LaterLayerWeaker {
                    earlier: a,
                    later: b,
                    outcomes: only_b,
                }
            } else {
                Classification::LaterLayerStronger {
                    earlier: a,
                    later: b,
                    outcomes: only_a,
                }
            }
        }
        (true, false) => {
            if only_b.is_empty() {
                Classification::Consistent
            } else {
                Classification::ObservedOutsidePrediction {
                    layer_pred: a,
                    layer_obs: b,
                    outcomes: only_b,
                }
            }
        }
        (false, true) => {
            if only_a.is_empty() {
                Classification::Consistent
            } else {
                Classification::ObservedOutsidePrediction {
                    layer_pred: b,
                    layer_obs: a,
                    outcomes: only_a,
                }
            }
        }
        (false, false) => {
            if only_a.is_empty() && only_b.is_empty() {
                Classification::Consistent
            } else {
                Classification::SampleCoverageDifference {
                    a,
                    b,
                    only_a,
                    only_b,
                }
            }
        }
    };
    Comparison {
        a,
        b,
        classification: cls,
    }
}

/// Per-layer summary of the *compiler* pipeline events for one thread, used to
/// localise where the ordering annotations change shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadPipeline {
    pub symbol: String,
    pub mir_built: Option<LayerEvents>,
    pub mir_optimized: Option<LayerEvents>,
    pub llvm_ir: Option<LayerEvents>,
    pub asm: Option<LayerEvents>,
    /// Orderings, in program order, at each comparable IR layer.
    pub ordering_chain: Vec<(String, Vec<String>)>,
    /// First adjacent pair of layers where the ordering sequence differs, if any.
    pub first_ordering_change: Option<(String, String)>,
    /// Full atomic event fingerprints at comparable IR layers. Unlike `ordering_chain`,
    /// these include the event kind, source location, ordering, and RMW/CAS details.
    /// Assembly stays outside this chain because its raw instructions do not carry the
    /// same source-level annotation vocabulary; the lifter handles that boundary.
    #[serde(default)]
    pub event_shape_chain: Vec<(String, Vec<String>)>,
    /// First adjacent pair of comparable IR layers whose full atomic event fingerprints
    /// differ. This catches location or operation changes that a pure ordering comparison
    /// cannot see.
    #[serde(default)]
    pub first_event_change: Option<(String, String)>,
}

fn orderings(ev: &LayerEvents) -> Vec<String> {
    ev.events
        .iter()
        .filter_map(|e| match e {
            Event::Load { ord, .. } => Some(format!("R.{ord}")),
            Event::Store { ord, .. } => Some(format!("W.{ord}")),
            Event::Rmw { op, ord, .. } => Some(format!("RMW.{op}.{ord}")),
            Event::Cmpxchg {
                success, failure, ..
            } => Some(format!("CAS.{success}/{failure}")),
            Event::Fence { ord } => Some(format!("F.{ord}")),
            Event::Asm { .. } => None,
        })
        .collect()
}

fn event_shapes(ev: &LayerEvents) -> Vec<String> {
    ev.events
        .iter()
        .filter_map(|e| match e {
            Event::Load { loc, ord } => Some(format!("R.{loc}.{ord}")),
            Event::Store { loc, ord } => Some(format!("W.{loc}.{ord}")),
            Event::Rmw { loc, op, ord } => Some(format!("RMW.{loc}.{op}.{ord}")),
            Event::Cmpxchg {
                loc,
                success,
                failure,
            } => Some(format!("CAS.{loc}.{success}/{failure}")),
            Event::Fence { ord } => Some(format!("F.{ord}")),
            Event::Asm { .. } => None,
        })
        .collect()
}

pub fn thread_pipeline(t: &crate::compile::ThreadArtifacts) -> ThreadPipeline {
    let mut chain = Vec::new();
    let mut shape_chain = Vec::new();
    if let Some(e) = &t.events_mir_built {
        chain.push(("mir_built".to_string(), orderings(e)));
        shape_chain.push(("mir_built".to_string(), event_shapes(e)));
    }
    if let Some(e) = &t.events_mir_optimized {
        chain.push(("mir_optimized".to_string(), orderings(e)));
        shape_chain.push(("mir_optimized".to_string(), event_shapes(e)));
    }
    if let Some(e) = &t.events_llvm_ir {
        chain.push(("llvm_ir".to_string(), orderings(e)));
        shape_chain.push(("llvm_ir".to_string(), event_shapes(e)));
    }
    let mut first_change = None;
    for w in chain.windows(2) {
        if w[0].1 != w[1].1 {
            first_change = Some((w[0].0.clone(), w[1].0.clone()));
            break;
        }
    }
    let mut first_event_change = None;
    for w in shape_chain.windows(2) {
        if w[0].1 != w[1].1 {
            first_event_change = Some((w[0].0.clone(), w[1].0.clone()));
            break;
        }
    }
    ThreadPipeline {
        symbol: t.symbol.clone(),
        mir_built: t.events_mir_built.clone(),
        mir_optimized: t.events_mir_optimized.clone(),
        llvm_ir: t.events_llvm_ir.clone(),
        asm: t.events_asm.clone(),
        ordering_chain: chain,
        first_ordering_change: first_change,
        event_shape_chain: shape_chain,
        first_event_change,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerOutcome {
    pub layer: Layer,
    pub status: Status,
    pub outcomes: Option<OutcomeSet>,
    pub tool: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Localization {
    /// Layers in pipeline order that produced outcome sets.
    pub chain: Vec<Layer>,
    /// Pairwise comparisons of adjacent layers in the chain.
    pub adjacent: Vec<Comparison>,
    /// Comparison of every prediction against the final observation.
    pub against_hardware: Vec<Comparison>,
    /// The first adjacent boundary whose classification is not `Consistent`, if any.
    pub earliest_divergence: Option<Comparison>,
    pub summary: String,
}

pub fn localize(layers: &[LayerOutcome]) -> Localization {
    localize_with_gap(layers, None)
}

/// Like [`localize`], but when a divergence against the source model is found and a
/// weakened source-model outcome set is supplied (same program, one axiom dropped), any
/// outcome that the weakened model admits is marked as diagnostic model compatibility.
/// This does not establish the cause of a compiler or hardware disagreement.
pub fn localize_with_gap(
    layers: &[LayerOutcome],
    weakened: Option<(&str, &OutcomeSet)>,
) -> Localization {
    let chain: Vec<Layer> = layers.iter().map(|l| l.layer).collect();
    let get = |l: Layer| {
        layers
            .iter()
            .find(|x| x.layer == l)
            .and_then(|x| x.outcomes.as_ref())
    };
    let explain = |c: Comparison| -> Comparison {
        let Some((axiom, weak)) = weakened else {
            return c;
        };
        let (earlier, later, outcomes) = match &c.classification {
            Classification::LaterLayerWeaker {
                earlier,
                later,
                outcomes,
            } if *earlier == Layer::SourceModel => (*earlier, *later, outcomes.clone()),
            Classification::ObservedOutsidePrediction {
                layer_pred,
                layer_obs,
                outcomes,
            } if *layer_pred == Layer::SourceModel => (*layer_pred, *layer_obs, outcomes.clone()),
            _ => return c,
        };
        if outcomes.iter().all(|o| weak.contains(o)) {
            Comparison {
                a: c.a,
                b: c.b,
                classification: Classification::ExplainedByModelGap {
                    earlier,
                    later,
                    axiom: axiom.to_string(),
                    outcomes,
                },
            }
        } else {
            c
        }
    };
    let mut adjacent = Vec::new();
    for w in chain.windows(2) {
        adjacent.push(explain(compare(w[0], get(w[0]), w[1], get(w[1]))));
    }
    let mut against_hardware = Vec::new();
    if chain.contains(&Layer::Hardware) {
        for &l in &chain {
            if l != Layer::Hardware {
                against_hardware.push(explain(compare(
                    l,
                    get(l),
                    Layer::Hardware,
                    get(Layer::Hardware),
                )));
            }
        }
    }
    let earliest = adjacent
        .iter()
        .find(|c| {
            !matches!(
                c.classification,
                Classification::Consistent | Classification::NotComparable { .. }
            )
        })
        .cloned();
    let summary = if chain.len() < 2 {
        match chain.first() {
            Some(layer) => format!("insufficient comparable layers (only {})", layer.name()),
            None => "insufficient comparable layers (no layer boundary was available)".to_string(),
        }
    } else {
        match &earliest {
            None => {
                let n_nc = adjacent
                    .iter()
                    .filter(|c| matches!(c.classification, Classification::NotComparable { .. }))
                    .count();
                if n_nc == 0 {
                    "all adjacent layers consistent".to_string()
                } else {
                    format!("no divergence among comparable layers ({n_nc} boundary/boundaries not comparable)")
                }
            }
            Some(c) => format!(
                "earliest divergence between {} and {}: {}",
                c.a.name(),
                c.b.name(),
                describe(&c.classification)
            ),
        }
    };
    Localization {
        chain,
        adjacent,
        against_hardware,
        earliest_divergence: earliest,
        summary,
    }
}

pub fn describe(c: &Classification) -> String {
    match c {
        Classification::Consistent => "consistent".into(),
        Classification::ObservedOutsidePrediction {
            layer_pred,
            layer_obs,
            outcomes,
        } => {
            format!(
                "{} observed {} outcome(s) that {} forbids: {}",
                layer_obs.name(),
                outcomes.len(),
                layer_pred.name(),
                outcomes
                    .iter()
                    .map(|o| format!("[{o}]"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        Classification::LaterLayerStronger {
            earlier,
            later,
            outcomes,
        } => {
            format!(
                "{} forbids {} outcome(s) that {} allows (mapping stronger than source model): {}",
                later.name(),
                outcomes.len(),
                earlier.name(),
                outcomes
                    .iter()
                    .map(|o| format!("[{o}]"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        Classification::LaterLayerWeaker {
            earlier,
            later,
            outcomes,
        } => {
            format!(
                "{} allows {} outcome(s) that {} forbids: {}",
                later.name(),
                outcomes.len(),
                earlier.name(),
                outcomes
                    .iter()
                    .map(|o| format!("[{o}]"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        Classification::SampleCoverageDifference {
            a,
            b,
            only_a,
            only_b,
        } => {
            format!(
                "sampled layers differ: only {}: {}; only {}: {}",
                a.name(),
                only_a.len(),
                b.name(),
                only_b.len()
            )
        }
        Classification::ExplainedByModelGap {
            earlier,
            later,
            axiom,
            outcomes,
        } => {
            format!("{} allows {} outcome(s) that {} forbids, all admitted by a diagnostic source-model variant with `{axiom}` removed: {}", later.name(), outcomes.len(), earlier.name(), outcomes.iter().map(|o| format!("[{o}]")).collect::<Vec<_>>().join(" "))
        }
        Classification::NotComparable { reason } => format!("not comparable: {reason}"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub generator: String,
    pub generation_reason: String,
    pub seed: Option<u64>,
    pub family: String,
    pub parent_case: Option<String>,
    pub created_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub schema_version: u32,
    pub case_id: String,
    pub litmus: Litmus,
    pub litmus_digest: String,
    pub provenance: Provenance,
    pub rust_source: String,
    pub rust_source_sha256: String,
    pub c11_litmus: String,
    pub compile: Option<CompileResult>,
    pub pipeline: Vec<ThreadPipeline>,
    pub lifted: Option<Lifted>,
    pub lift_error: Option<LiftError>,
    pub herd_source: Option<HerdResult>,
    /// Nonstandard source model with the no-thin-air axiom removed; retained as diagnostic
    /// context and never used to predict or reclassify a finding.
    pub herd_source_weak: Option<HerdResult>,
    pub herd_arch: Option<HerdResult>,
    pub miri_weak: Option<MiriResult>,
    pub miri_genmc: Option<MiriResult>,
    pub hardware: Option<HardwareResult>,
    pub layers: Vec<LayerOutcome>,
    pub localization: Localization,
    pub limitations: Vec<String>,
    pub redactions: Vec<String>,
    pub replay: Vec<String>,
}

impl Bundle {
    pub fn config(&self) -> Option<&CompileConfig> {
        self.compile.as_ref().map(|c| &c.config)
    }

    /// Deterministic JSON (BTreeMap-ordered, `preserve_order` for structs).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("bundle serialises")
    }

    pub fn from_json(s: &str) -> Result<Self, String> {
        let b: Bundle = serde_json::from_str(s).map_err(|e| format!("bundle parse error: {e}"))?;
        if b.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported bundle schema version {}",
                b.schema_version
            ));
        }
        if b.litmus.digest() != b.litmus_digest {
            return Err("bundle corrupted: litmus digest mismatch".into());
        }
        if sha256_hex(b.rust_source.as_bytes()) != b.rust_source_sha256 {
            return Err("bundle corrupted: rust source digest mismatch".into());
        }
        Ok(b)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Strip anything that looks like a credential from a free-text blob (tool stderr, env
/// dumps). Returns the redacted text and the list of redaction reasons.
pub fn redact(text: &str) -> (String, Vec<String>) {
    let mut reasons = Vec::new();
    let re = regex::Regex::new(r"(?i)\b([A-Z0-9_]*(TOKEN|SECRET|PASSWORD|PASSWD|API_?KEY|CREDENTIAL|AUTH)[A-Z0-9_]*)\s*[=:]\s*\S+").unwrap();
    let out = re
        .replace_all(text, |c: &regex::Captures| {
            reasons.push(format!("redacted value of {}", &c[1]));
            format!("{}=<redacted>", &c[1])
        })
        .into_owned();
    // GitHub-style tokens.
    let re2 = regex::Regex::new(r"\b(gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b")
        .unwrap();
    let out = re2
        .replace_all(&out, |_: &regex::Captures| {
            reasons.push("redacted GitHub token".into());
            "<redacted-token>".to_string()
        })
        .into_owned();
    (out, reasons)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&[&[u32]]], ex: bool) -> OutcomeSet {
        OutcomeSet {
            outcomes: v
                .iter()
                .map(|o| Outcome(o.iter().map(|r| r.to_vec()).collect()))
                .collect(),
            exhaustive: ex,
        }
    }

    #[test]
    fn pipeline_flags_location_change_even_when_orderings_match() {
        let mir_events = LayerEvents {
            events: vec![
                Event::Store {
                    loc: "loc0".into(),
                    ord: "relaxed".into(),
                },
                Event::Load {
                    loc: "loc1".into(),
                    ord: "relaxed".into(),
                },
            ],
            unparsed: Vec::new(),
        };
        let llvm_events = LayerEvents {
            events: vec![
                Event::Store {
                    loc: "loc1".into(),
                    ord: "relaxed".into(),
                },
                Event::Load {
                    loc: "loc0".into(),
                    ord: "relaxed".into(),
                },
            ],
            unparsed: Vec::new(),
        };
        let thread = crate::compile::ThreadArtifacts {
            symbol: "rl_thread_0".into(),
            mir_built: None,
            mir_optimized: None,
            llvm_ir: None,
            asm: None,
            events_mir_built: Some(mir_events.clone()),
            events_mir_optimized: Some(mir_events),
            events_llvm_ir: Some(llvm_events),
            events_asm: None,
        };

        let pipeline = thread_pipeline(&thread);
        assert_eq!(
            pipeline
                .ordering_chain
                .iter()
                .map(|(layer, _)| layer.as_str())
                .collect::<Vec<_>>(),
            vec!["mir_built", "mir_optimized", "llvm_ir"]
        );
        assert!(pipeline.first_ordering_change.is_none());
        assert_eq!(
            pipeline.first_event_change,
            Some(("mir_optimized".into(), "llvm_ir".into()))
        );
    }

    #[test]
    fn classifies_observed_outside_prediction() {
        let pred = set(&[&[&[0], &[1]], &[&[1], &[0]], &[&[1], &[1]]], true);
        let obs = set(&[&[&[0], &[0]], &[&[1], &[0]]], false);
        let c = compare(Layer::SourceModel, Some(&pred), Layer::Hardware, Some(&obs));
        match c.classification {
            Classification::ObservedOutsidePrediction { outcomes, .. } => {
                assert_eq!(outcomes, vec![Outcome(vec![vec![0], vec![0]])])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn follow_up_excludes_expected_stronger_mappings() {
        let outcome = Outcome(vec![vec![0]]);
        assert!(!requires_follow_up(&Classification::LaterLayerStronger {
            earlier: Layer::SourceModel,
            later: Layer::ArchModel,
            outcomes: vec![outcome.clone()],
        }));
        assert!(requires_follow_up(&Classification::LaterLayerWeaker {
            earlier: Layer::SourceModel,
            later: Layer::ArchModel,
            outcomes: vec![outcome.clone()],
        }));
        assert!(requires_follow_up(
            &Classification::SampleCoverageDifference {
                a: Layer::SourceEmulator,
                b: Layer::Hardware,
                only_a: vec![outcome],
                only_b: Vec::new(),
            }
        ));
    }

    #[test]
    fn classifies_exhaustive_pairs() {
        let src = set(
            &[&[&[0], &[0]], &[&[0], &[1]], &[&[1], &[0]], &[&[1], &[1]]],
            true,
        );
        let arch = set(&[&[&[0], &[1]], &[&[1], &[0]], &[&[1], &[1]]], true);
        let c = compare(
            Layer::SourceModel,
            Some(&src),
            Layer::ArchModel,
            Some(&arch),
        );
        assert!(matches!(
            c.classification,
            Classification::LaterLayerStronger { .. }
        ));
        let c = compare(
            Layer::SourceModel,
            Some(&arch),
            Layer::ArchModel,
            Some(&src),
        );
        assert!(matches!(
            c.classification,
            Classification::LaterLayerWeaker { .. }
        ));
        let c = compare(Layer::SourceModel, Some(&src), Layer::ArchModel, Some(&src));
        assert!(matches!(c.classification, Classification::Consistent));
    }

    #[test]
    fn not_comparable_when_missing() {
        let c = compare(
            Layer::SourceModel,
            None,
            Layer::Hardware,
            Some(&set(&[], false)),
        );
        assert!(matches!(
            c.classification,
            Classification::NotComparable { .. }
        ));
    }

    #[test]
    fn localize_finds_earliest() {
        let full = set(
            &[&[&[0], &[0]], &[&[0], &[1]], &[&[1], &[0]], &[&[1], &[1]]],
            true,
        );
        let sc = set(&[&[&[0], &[1]], &[&[1], &[0]], &[&[1], &[1]]], true);
        let layers = vec![
            LayerOutcome {
                layer: Layer::SourceModel,
                status: Status::Predicted,
                outcomes: Some(full.clone()),
                tool: "t".into(),
                notes: vec![],
            },
            LayerOutcome {
                layer: Layer::ArchModel,
                status: Status::Predicted,
                outcomes: Some(sc.clone()),
                tool: "t".into(),
                notes: vec![],
            },
            LayerOutcome {
                layer: Layer::Hardware,
                status: Status::Observed,
                outcomes: Some(set(&[&[&[0], &[1]], &[&[1], &[0]]], false)),
                tool: "t".into(),
                notes: vec![],
            },
        ];
        let l = localize(&layers);
        let e = l.earliest_divergence.unwrap();
        assert_eq!((e.a, e.b), (Layer::SourceModel, Layer::ArchModel));
        assert!(matches!(
            e.classification,
            Classification::LaterLayerStronger { .. }
        ));
        assert_eq!(l.against_hardware.len(), 2);
    }

    #[test]
    fn single_layer_is_not_reported_consistent() {
        let result = localize(&[LayerOutcome {
            layer: Layer::Hardware,
            status: Status::Observed,
            outcomes: Some(OutcomeSet {
                outcomes: Vec::new(),
                exhaustive: false,
            }),
            tool: "test".into(),
            notes: Vec::new(),
        }]);
        assert!(result.summary.starts_with("insufficient comparable layers"));
        assert!(result.earliest_divergence.is_none());
    }

    #[test]
    fn redacts_secrets() {
        let (out, reasons) = redact(
            "GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123 and API_KEY: sk-live-1234 done",
        );
        assert!(!out.contains("ghp_abc"));
        assert!(!out.contains("sk-live"));
        assert!(reasons.len() >= 2, "{reasons:?}");
        let (out, reasons) = redact("PATH=/usr/bin");
        assert_eq!(out, "PATH=/usr/bin");
        assert!(reasons.is_empty());
    }
}
