//! End-to-end pipeline for one case under one configuration, producing a [`Bundle`].

use crate::compile::{compile, CompileConfig};
use crate::evidence::{
    redact, sha256_hex, thread_pipeline, Bundle, Layer, LayerOutcome, Provenance, Status,
    SCHEMA_VERSION,
};
use crate::hardware::run_binary;
use crate::herd::run_herd;
use crate::lift::lift;
use crate::litmus::Litmus;
use crate::miri::{run_genmc, run_weak_memory, MiriConfig};
use crate::render_c11;
use crate::render_rust;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Tools {
    pub herd: Option<PathBuf>,
    pub miri: Option<MiriConfig>,
    /// Miri built with GenMC support (may be the same driver, or a separate build).
    pub miri_genmc: Option<MiriConfig>,
    /// User-mode emulator for foreign targets (e.g. `qemu-aarch64`); results are labelled
    /// emulated and never count as hardware.
    pub emulator: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Budget {
    pub compile_timeout: Duration,
    pub herd_timeout: Duration,
    pub miri_timeout: Duration,
    pub hw_timeout: Duration,
    pub hw_batches: usize,
    pub hw_iters: usize,
    pub miri_seeds: Vec<u64>,
    pub miri_iters: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            compile_timeout: Duration::from_secs(300),
            herd_timeout: Duration::from_secs(120),
            miri_timeout: Duration::from_secs(300),
            hw_timeout: Duration::from_secs(120),
            hw_batches: 3,
            hw_iters: 20_000,
            miri_seeds: (0..4).collect(),
            miri_iters: 50,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stages {
    pub herd_source: bool,
    pub compile: bool,
    pub herd_arch: bool,
    pub miri_weak: bool,
    pub miri_genmc: bool,
    pub hardware: bool,
}

impl Stages {
    pub fn all() -> Self {
        Stages {
            herd_source: true,
            compile: true,
            herd_arch: true,
            miri_weak: true,
            miri_genmc: true,
            hardware: true,
        }
    }
}

/// Inputs that stay constant while one case moves through the pipeline.
pub struct RunContext<'a> {
    pub cfg: &'a CompileConfig,
    pub tools: &'a Tools,
    pub budget: &'a Budget,
    pub stages: &'a Stages,
    pub source_model: &'a str,
}

/// Architecture `.cat` model for a target.
pub fn arch_model_for(target: &str) -> Option<&'static str> {
    if target.starts_with("aarch64") {
        Some("aarch64.cat")
    } else if target.starts_with("x86_64") {
        Some("x86tso-mixed.cat")
    } else {
        None
    }
}

/// The weakened companion of a source model (same file name with `-nooota` inserted before
/// the extension), if it exists on disk. Only our in-repo models have companions.
pub fn weakened_model_for(source_model: &str) -> Option<String> {
    let p = Path::new(source_model);
    let stem = p.file_stem()?.to_str()?;
    let ext = p.extension()?.to_str()?;
    let candidate = p.with_file_name(format!("{stem}-nooota.{ext}"));
    candidate.is_file().then(|| candidate.display().to_string())
}

pub fn run_case(
    litmus: &Litmus,
    context: RunContext<'_>,
    work: &Path,
    provenance: Provenance,
) -> Result<Bundle, String> {
    let cfg = context.cfg;
    let tools = context.tools;
    let budget = context.budget;
    let stages = context.stages;
    let source_model = context.source_model;
    litmus.validate()?;
    std::fs::create_dir_all(work).map_err(|e| format!("create {}: {e}", work.display()))?;
    let rendered = render_rust::render(litmus);
    let src_path = work.join("case.rs");
    std::fs::write(&src_path, &rendered.source).map_err(|e| e.to_string())?;
    let c11 = render_c11::render(litmus);
    let c11_path = work.join("case.c11.litmus");
    std::fs::write(&c11_path, &c11).map_err(|e| e.to_string())?;

    let mut limitations = Vec::new();
    let mut redactions = Vec::new();
    let mut layers = Vec::new();
    let mut replay = Vec::new();

    // Layer: source model (herd7 rc11).
    let herd_source = if stages.herd_source {
        match &tools.herd {
            Some(h) => {
                let dec = |l: &str| crate::herd::parse_state_line(l, litmus);
                let r = run_herd(h, source_model, &c11_path, Some(&dec), budget.herd_timeout);
                replay.push(r.command.join(" "));
                layers.push(LayerOutcome {
                    layer: Layer::SourceModel,
                    status: if r.outcomes.is_some() {
                        Status::Predicted
                    } else {
                        Status::Unsupported
                    },
                    outcomes: r.outcomes.clone(),
                    tool: format!("{} {} model={}", r.tool, r.version, r.model),
                    notes: r.warnings.clone(),
                });
                Some(r)
            }
            None => {
                limitations.push("herd7 not available: no source-model prediction".into());
                None
            }
        }
    } else {
        None
    };
    // A nonstandard source-model variant with the no-thin-air axiom dropped. It is kept as
    // diagnostic context only and never changes the localization classification.
    let herd_source_weak = match (&herd_source, &tools.herd, weakened_model_for(source_model)) {
        (Some(_), Some(h), Some(weak_model)) => {
            let dec = |l: &str| crate::herd::parse_state_line(l, litmus);
            let r = run_herd(h, &weak_model, &c11_path, Some(&dec), budget.herd_timeout);
            replay.push(r.command.join(" "));
            Some(r)
        }
        _ => None,
    };

    // Layer: Miri weak-memory emulation (sampled).
    let miri_weak = if stages.miri_weak {
        match &tools.miri {
            Some(m) => {
                let r = run_weak_memory(
                    m,
                    &src_path,
                    &work.join("miri"),
                    &budget.miri_seeds,
                    budget.miri_iters,
                    budget.miri_timeout,
                );
                if let Some(first) = r.runs.first() {
                    replay.push(first.command.join(" "));
                }
                layers.push(LayerOutcome {
                    layer: Layer::SourceEmulator,
                    status: if r.outcomes.is_some() {
                        Status::Observed
                    } else {
                        Status::Unsupported
                    },
                    outcomes: r.outcomes.clone(),
                    tool: format!("{} {} ({})", r.tool, r.version, r.mode),
                    notes: r.warnings.clone(),
                });
                Some(r)
            }
            None => {
                limitations.push("miri not available".into());
                None
            }
        }
    } else {
        None
    };

    // Layer: Miri-GenMC (exhaustive at MIR level).
    let miri_genmc = if stages.miri_genmc {
        match &tools.miri_genmc {
            Some(m) => {
                let single = render_rust::render_single(litmus);
                let single_path = work.join("case_single.rs");
                std::fs::write(&single_path, &single).map_err(|e| e.to_string())?;
                let r = run_genmc(m, &single_path, &work.join("genmc"), budget.miri_timeout);
                if let Some(first) = r.runs.first() {
                    replay.push(first.command.join(" "));
                }
                layers.push(LayerOutcome {
                    layer: Layer::SourceModelChecker,
                    status: if r.outcomes.is_some() {
                        Status::Predicted
                    } else {
                        Status::Unsupported
                    },
                    outcomes: r.outcomes.clone(),
                    tool: format!(
                        "{} {} ({}, {} executions)",
                        r.tool,
                        r.version,
                        r.mode,
                        r.explored_executions
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "?".into())
                    ),
                    notes: r.warnings.clone(),
                });
                Some(r)
            }
            None => {
                limitations.push(
                    "Miri-GenMC not available (requires a Miri built with --features=genmc)".into(),
                );
                None
            }
        }
    } else {
        None
    };

    // Compile + capture.
    let compile_res = if stages.compile || stages.herd_arch || stages.hardware {
        match compile(
            &src_path,
            &work.join("build"),
            cfg,
            &rendered.thread_symbols,
            budget.compile_timeout,
        ) {
            Ok(mut c) => {
                let (s, r) = redact(&c.stderr);
                c.stderr = s;
                redactions.extend(r);
                replay.push(c.command.join(" "));
                if c.exit_code != Some(0) {
                    limitations.push(format!(
                        "compilation failed (exit {:?}); see compile.stderr",
                        c.exit_code
                    ));
                }
                Some(c)
            }
            Err(e) => {
                limitations.push(format!("compile error: {e}"));
                None
            }
        }
    } else {
        None
    };
    let pipeline: Vec<_> = compile_res
        .as_ref()
        .map(|c| c.threads.iter().map(thread_pipeline).collect())
        .unwrap_or_default();

    // Layer: architecture model on lifted assembly.
    let (lifted, lift_error, herd_arch) = if stages.herd_arch {
        match (&compile_res, &tools.herd, arch_model_for(&cfg.target)) {
            (Some(c), _, _) if c.exit_code != Some(0) => (None, None, None),
            (Some(c), Some(h), Some(model)) => {
                let asm: Vec<Option<String>> = c.threads.iter().map(|t| t.asm.clone()).collect();
                match lift(litmus, cfg, &asm) {
                    Ok(l) => {
                        let p = work.join("build").join("lifted.litmus");
                        std::fs::write(&p, &l.litmus_text).map_err(|e| e.to_string())?;
                        let dec = |s: &str| crate::lift::decode_state(s, &l.reg_map);
                        let r = run_herd(h, model, &p, Some(&dec), budget.herd_timeout);
                        replay.push(r.command.join(" "));
                        layers.push(LayerOutcome {
                            layer: Layer::ArchModel,
                            status: if r.outcomes.is_some() {
                                Status::Predicted
                            } else {
                                Status::Unsupported
                            },
                            outcomes: r.outcomes.clone(),
                            tool: format!(
                                "{} {} model={} (lifted from {} asm)",
                                r.tool,
                                r.version,
                                r.model,
                                cfg.label()
                            ),
                            notes: r.warnings.clone(),
                        });
                        (Some(l), None, Some(r))
                    }
                    Err(e) => {
                        limitations.push(format!("assembly lifting unsupported: {e}"));
                        layers.push(LayerOutcome {
                            layer: Layer::ArchModel,
                            status: Status::Unsupported,
                            outcomes: None,
                            tool: "lifter".into(),
                            notes: vec![e.to_string()],
                        });
                        (None, Some(e), None)
                    }
                }
            }
            (None, _, _) => (None, None, None),
            (_, None, _) => {
                limitations.push("herd7 not available: no architecture-model prediction".into());
                (None, None, None)
            }
            (_, _, None) => {
                limitations.push(format!("no architecture model for target {}", cfg.target));
                (None, None, None)
            }
        }
    } else {
        (None, None, None)
    };

    // Layer: hardware (or labelled emulation).
    let hardware = if stages.hardware {
        match compile_res
            .as_ref()
            .filter(|c| c.exit_code == Some(0))
            .and_then(|c| c.binary.clone())
        {
            Some(bin) => {
                let native = cfg.target.starts_with(std::env::consts::ARCH)
                    || (cfg.target.starts_with("x86_64") && std::env::consts::ARCH == "x86_64");
                let emu = if native {
                    None
                } else {
                    tools.emulator.as_deref()
                };
                if !native && emu.is_none() {
                    limitations.push(format!(
                        "target {} is not native and no emulator configured: no execution",
                        cfg.target
                    ));
                    None
                } else {
                    let r = run_binary(
                        &bin,
                        emu,
                        budget.hw_batches,
                        budget.hw_iters,
                        budget.hw_timeout,
                    );
                    replay.push(r.command.join(" "));
                    let mut notes = r.warnings.clone();
                    if r.emulated {
                        notes.push(
                            "EMULATED via user-mode emulator: this is NOT a hardware observation"
                                .into(),
                        );
                    }
                    layers.push(LayerOutcome {
                        layer: Layer::Hardware,
                        status: if r.emulated {
                            Status::Inferred
                        } else if r.outcomes.is_some() {
                            Status::Observed
                        } else {
                            Status::Unknown
                        },
                        outcomes: r.outcomes.clone(),
                        tool: if r.emulated {
                            "user-mode emulation".into()
                        } else {
                            format!(
                                "native {} ({})",
                                r.host.arch,
                                r.host
                                    .cpu_model
                                    .clone()
                                    .unwrap_or_else(|| "unknown cpu".into())
                            )
                        },
                        notes,
                    });
                    Some(r)
                }
            }
            None => None,
        }
    } else {
        None
    };

    // Order layers in pipeline order for localisation.
    layers.sort_by_key(|l| l.layer);
    let localization = crate::evidence::localize(&layers);
    if herd_source_weak.is_some() {
        limitations.push("A nonstandard source-model variant without the no-thin-air axiom was evaluated as diagnostic context; it does not alter classification.".into());
    }
    limitations.push(format!(
        "Source model is herd7 `{source_model}`{}; it retains RC11's `acyclic (sb | rf)` no-thin-air axiom. For load-buffering-shaped cases, compare its exclusions against an architecture model or observed hardware before drawing a mapping conclusion.",
        if source_model.contains("p0982") {
            " (P0982 release sequences: only RMWs continue; Rust documents its atomic rules as C++20)"
        } else {
            " (stock RC11 release-sequence semantics)"
        }
    ));
    limitations
        .push("Hardware outcome sets are finite samples: absence is not impossibility.".into());

    let case_id = format!("{}-{}", litmus.name, &litmus.digest()[..12]);
    Ok(Bundle {
        schema_version: SCHEMA_VERSION,
        case_id,
        litmus: litmus.clone(),
        litmus_digest: litmus.digest(),
        provenance,
        rust_source_sha256: sha256_hex(rendered.source.as_bytes()),
        rust_source: rendered.source,
        c11_litmus: c11,
        compile: compile_res,
        pipeline,
        lifted,
        lift_error,
        herd_source,
        herd_source_weak,
        herd_arch,
        miri_weak,
        miri_genmc,
        hardware,
        layers,
        localization,
        limitations,
        redactions,
        replay,
    })
}

pub fn now_utc() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // Civil date from days since epoch (proleptic Gregorian), no external crate.
    let secs = d.as_secs();
    let days = secs / 86400;
    let (y, m, dd) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}Z",
        (secs % 86400) / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn civil_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_968), (2024, 9, 2));
    }
}
