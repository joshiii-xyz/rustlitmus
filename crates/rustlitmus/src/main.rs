use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use rustlitmus::compile::CompileConfig;
use rustlitmus::evidence::{Bundle, Classification, Provenance};
use rustlitmus::families::{parse_case_name, Family};
use rustlitmus::miri::MiriConfig;
use rustlitmus::pipeline::{now_utc, run_case, Budget, RunContext, Stages, Tools};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_SWEEP_MAX_CASES: usize = 32;

#[derive(Parser)]
#[command(
    name = "rustlitmus",
    version,
    about = "Cross-layer semantic evidence and localisation for concurrent Rust"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct ToolArgs {
    /// Path to herd7 (default: found on PATH).
    #[arg(long)]
    herd: Option<PathBuf>,
    /// Path to the Miri driver binary for weak-memory emulation.
    #[arg(long)]
    miri: Option<PathBuf>,
    /// Path to a Miri driver built with `--features=genmc`.
    #[arg(long)]
    miri_genmc: Option<PathBuf>,
    /// Miri sysroot (`--sysroot`).
    #[arg(long)]
    miri_sysroot: Option<PathBuf>,
    /// User-mode emulator for foreign targets (results labelled emulated).
    #[arg(long)]
    emulator: Option<PathBuf>,
}

impl ToolArgs {
    fn tools(&self) -> Tools {
        let herd = self
            .herd
            .clone()
            .or_else(|| rustlitmus::process::which("herd7"));
        let sysroot = self
            .miri_sysroot
            .clone()
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/miri")));
        let mk = |d: &Option<PathBuf>| match (d, &sysroot) {
            (Some(d), Some(s)) => Some(MiriConfig {
                driver: d.clone(),
                sysroot: s.clone(),
                extra_flags: vec![],
            }),
            _ => None,
        };
        Tools {
            herd,
            miri: mk(&self.miri),
            miri_genmc: mk(&self.miri_genmc),
            emulator: self.emulator.clone(),
        }
    }
}

#[derive(clap::Args, Clone)]
struct ConfigArgs {
    /// rustup toolchain name (`stable`, `nightly`, `1.85.0`, ...).
    #[arg(long, default_value = "stable")]
    toolchain: String,
    #[arg(long, default_value = "x86_64-unknown-linux-gnu")]
    target: String,
    #[arg(long, default_value = "3")]
    opt_level: String,
    /// Extra rustc flags (repeatable), e.g. `-Ctarget-feature=+lse`.
    #[arg(long = "rustc-flag")]
    rustc_flags: Vec<String>,
}

impl ConfigArgs {
    fn config(&self) -> CompileConfig {
        let mut extra = self.rustc_flags.clone();
        // AArch64: keep atomics inline so the lifter can see them, and use the cross linker.
        if self.target.starts_with("aarch64") {
            if !extra.iter().any(|f| f.contains("outline-atomics")) {
                extra.push("-Ctarget-feature=-outline-atomics".into());
            }
            if !extra.iter().any(|f| f.starts_with("-Clinker"))
                && std::env::consts::ARCH != "aarch64"
            {
                if let Some(cc) = rustlitmus::process::which("aarch64-linux-gnu-gcc") {
                    extra.push(format!("-Clinker={}", cc.display()));
                }
            }
        }
        CompileConfig {
            toolchain: self.toolchain.clone(),
            target: self.target.clone(),
            opt_level: self.opt_level.clone(),
            extra_flags: extra,
        }
    }
}

#[derive(clap::Args, Clone)]
struct BudgetArgs {
    #[arg(long, default_value_t = 3)]
    hw_batches: usize,
    #[arg(long, default_value_t = 20000)]
    hw_iters: usize,
    #[arg(long, default_value_t = 4)]
    miri_seeds: u64,
    #[arg(long, default_value_t = 50)]
    miri_iters: usize,
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

impl BudgetArgs {
    fn budget(&self) -> Budget {
        Budget {
            compile_timeout: Duration::from_secs(self.timeout_secs),
            herd_timeout: Duration::from_secs(self.timeout_secs),
            miri_timeout: Duration::from_secs(self.timeout_secs),
            hw_timeout: Duration::from_secs(self.timeout_secs),
            hw_batches: self.hw_batches,
            hw_iters: self.hw_iters,
            miri_seeds: (0..self.miri_seeds).collect(),
            miri_iters: self.miri_iters,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List families and instance counts.
    Families,
    /// Print the Rust program and C11 litmus for a case name (e.g. `SB+rlx-rlx-rlx-rlx`).
    Render {
        case: String,
        #[arg(long, default_value = "rust")]
        what: String,
    },
    /// Run the full pipeline on one case and write an evidence bundle.
    Run {
        case: String,
        #[arg(long, default_value = "out")]
        out: PathBuf,
        #[command(flatten)]
        tools: ToolArgs,
        #[command(flatten)]
        cfg: ConfigArgs,
        #[command(flatten)]
        budget: BudgetArgs,
        /// Skip stages: comma-separated subset of herd-source,compile,herd-arch,miri-weak,miri-genmc,hardware.
        #[arg(long, default_value = "")]
        skip: String,
        /// herd7 source-level model file.
        #[arg(long, default_value = "models/rc11-p0982.cat")]
        source_model: String,
        /// Generation reason recorded in provenance.
        #[arg(long, default_value = "manual")]
        reason: String,
    },
    /// Run every ordering instance of a family (or all families) and summarise divergences.
    Sweep {
        /// Family name or `all`.
        family: String,
        #[arg(long, default_value = "out")]
        out: PathBuf,
        #[command(flatten)]
        tools: ToolArgs,
        #[command(flatten)]
        cfg: ConfigArgs,
        #[command(flatten)]
        budget: BudgetArgs,
        #[arg(long, default_value = "miri-weak,miri-genmc")]
        skip: String,
        #[arg(long, default_value = "models/rc11-p0982.cat")]
        source_model: String,
        /// Stop after this many cases. A finite cap is required for every campaign.
        #[arg(long, default_value_t = DEFAULT_SWEEP_MAX_CASES)]
        max_cases: usize,
        /// Secondary wall-clock cap checked between cases (0 disables this secondary cap).
        #[arg(long, default_value_t = 0)]
        max_secs: u64,
    },
    /// Verify and summarise an evidence bundle.
    Inspect { bundle: PathBuf },
    /// Re-run a preserved bundle's case with the same configuration and compare outcome sets.
    Replay {
        bundle: PathBuf,
        #[arg(long, default_value = "out/replay")]
        out: PathBuf,
        #[command(flatten)]
        tools: ToolArgs,
        #[command(flatten)]
        budget: BudgetArgs,
        #[arg(long, default_value = "")]
        skip: String,
        /// herd7 source-level model (default: the model recorded in the bundle).
        #[arg(long)]
        source_model: Option<String>,
    },
}

fn parse_skip(s: &str) -> Stages {
    let mut st = Stages::all();
    for item in s.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        match item {
            "herd-source" => st.herd_source = false,
            "compile" => st.compile = false,
            "herd-arch" => st.herd_arch = false,
            "miri-weak" => st.miri_weak = false,
            "miri-genmc" => st.miri_genmc = false,
            "hardware" => st.hardware = false,
            other => eprintln!("warning: unknown stage {other:?} in --skip"),
        }
    }
    st
}

fn litmus_for(case: &str) -> Result<rustlitmus::litmus::Litmus> {
    if let Some(l) = parse_case_name(case) {
        return Ok(l);
    }
    let p = PathBuf::from(case);
    if p.is_file() {
        let s = std::fs::read_to_string(&p)?;
        let l: rustlitmus::litmus::Litmus =
            serde_json::from_str(&s).context("parse litmus JSON")?;
        l.validate().map_err(|e| anyhow!(e))?;
        return Ok(l);
    }
    bail!("unknown case {case:?}: expected FAMILY+ord-ord-... or a path to a litmus JSON file")
}

fn validate_sweep_budget(max_cases: usize) -> Result<()> {
    if max_cases == 0 {
        bail!("--max-cases must be positive; choose a finite campaign cap")
    }
    Ok(())
}

fn summarise(b: &Bundle) -> String {
    let mut s = String::new();
    s.push_str(&format!("case {}  ({})\n", b.case_id, b.litmus.name));
    if let Some(c) = &b.compile {
        s.push_str(&format!(
            "config {}  rustc {}  LLVM {}\n",
            c.config.label(),
            c.toolchain.rustc_version,
            c.toolchain.llvm_version.as_deref().unwrap_or("?")
        ));
    }
    for l in &b.layers {
        let set = match &l.outcomes {
            Some(o) => format!(
                "{}{}",
                o.outcomes
                    .iter()
                    .map(|x| format!("[{x}]"))
                    .collect::<Vec<_>>()
                    .join(" "),
                if o.exhaustive {
                    " (exhaustive)"
                } else {
                    " (sampled)"
                }
            ),
            None => "—".to_string(),
        };
        s.push_str(&format!(
            "  {:<42} {:?}: {}\n",
            l.layer.name(),
            l.status,
            set
        ));
        for n in &l.notes {
            s.push_str(&format!("      note: {n}\n"));
        }
    }
    s.push_str(&format!("localization: {}\n", b.localization.summary));
    let sc = rustlitmus::score::score(b);
    s.push_str(&format!("score: {:.1} ({:?})\n", sc.total, sc.bucket));
    for c in &b.localization.against_hardware {
        if !matches!(c.classification, Classification::Consistent) {
            s.push_str(&format!(
                "  vs hardware: {} — {}\n",
                c.a.name(),
                rustlitmus::evidence::describe(&c.classification)
            ));
        }
    }
    for tp in &b.pipeline {
        if let Some((a, bb)) = &tp.first_event_change {
            s.push_str(&format!(
                "  {}: atomic event shape changes between {a} and {bb}: {:?}\n",
                tp.symbol, tp.event_shape_chain
            ));
        } else if let Some((a, bb)) = &tp.first_ordering_change {
            s.push_str(&format!(
                "  {}: ordering annotations change between {a} and {bb}: {:?}\n",
                tp.symbol, tp.ordering_chain
            ));
        }
    }
    if let Some(e) = &b.lift_error {
        s.push_str(&format!("lift error: {e}\n"));
    }
    for l in &b.limitations {
        s.push_str(&format!("  limitation: {l}\n"));
    }
    s
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Families => {
            for f in Family::ALL {
                println!(
                    "{:<12} slots={} instances={}",
                    f.name(),
                    f.slots(),
                    f.all_instances().len()
                );
            }
        }
        Cmd::Render { case, what } => {
            let l = litmus_for(&case)?;
            match what.as_str() {
                "rust" => print!("{}", rustlitmus::render_rust::render(&l).source),
                "c11" => print!("{}", rustlitmus::render_c11::render(&l)),
                "json" => println!("{}", serde_json::to_string_pretty(&l)?),
                _ => bail!("--what must be rust, c11 or json"),
            }
        }
        Cmd::Run {
            case,
            out,
            tools,
            cfg,
            budget,
            skip,
            source_model,
            reason,
        } => {
            let l = litmus_for(&case)?;
            let cfg = cfg.config();
            let work = out.join(&l.name).join(cfg.label());
            let prov = Provenance {
                generator: "cli".into(),
                generation_reason: reason,
                seed: None,
                family: l.name.split('+').next().unwrap_or("").into(),
                parent_case: None,
                created_utc: now_utc(),
            };
            let tools = tools.tools();
            let budget = budget.budget();
            let stages = parse_skip(&skip);
            let context = RunContext {
                cfg: &cfg,
                tools: &tools,
                budget: &budget,
                stages: &stages,
                source_model: &source_model,
            };
            let b = run_case(&l, context, &work, prov).map_err(|e| anyhow!(e))?;
            let path = work.join("bundle.json");
            std::fs::write(&path, b.to_json())?;
            print!("{}", summarise(&b));
            println!("bundle: {}", path.display());
        }
        Cmd::Sweep {
            family,
            out,
            tools,
            cfg,
            budget,
            skip,
            source_model,
            max_cases,
            max_secs,
        } => {
            validate_sweep_budget(max_cases)?;
            let fams: Vec<Family> = if family == "all" {
                Family::ALL.to_vec()
            } else {
                family
                    .split(',')
                    .map(|f| {
                        Family::from_name(f.trim()).ok_or_else(|| anyhow!("unknown family {f}"))
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            let cfg = cfg.config();
            let tools = tools.tools();
            let budget = budget.budget();
            let stages = parse_skip(&skip);
            let start = std::time::Instant::now();
            let mut n = 0usize;
            let mut divergent = Vec::new();
            let mut unsupported = 0usize;
            let index_path = out.join("sweep-index.jsonl");
            std::fs::create_dir_all(&out)?;
            let mut index = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&index_path)?;
            'outer: for f in fams {
                for l in f.all_instances() {
                    if n >= max_cases {
                        break 'outer;
                    }
                    if max_secs > 0 && start.elapsed().as_secs() >= max_secs {
                        eprintln!("sweep: wall-clock cap reached after {n} cases");
                        break 'outer;
                    }
                    n += 1;
                    let work = out.join(&l.name).join(cfg.label());
                    // Resumable: skip cases that already have a bundle for this config.
                    if work.join("bundle.json").is_file() {
                        continue;
                    }
                    let prov = Provenance {
                        generator: "sweep".into(),
                        generation_reason: format!(
                            "exhaustive ordering enumeration of family {}",
                            f.name()
                        ),
                        seed: None,
                        family: f.name().into(),
                        parent_case: None,
                        created_utc: now_utc(),
                    };
                    let context = RunContext {
                        cfg: &cfg,
                        tools: &tools,
                        budget: &budget,
                        stages: &stages,
                        source_model: &source_model,
                    };
                    let b = match run_case(&l, context, &work, prov) {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("{}: error: {e}", l.name);
                            continue;
                        }
                    };
                    std::fs::write(work.join("bundle.json"), b.to_json())?;
                    let needs_follow_up = b
                        .localization
                        .adjacent
                        .iter()
                        .chain(b.localization.against_hardware.iter())
                        .any(|c| rustlitmus::evidence::requires_follow_up(&c.classification));
                    let hw_flags: Vec<String> = b
                        .localization
                        .against_hardware
                        .iter()
                        .filter(|c| {
                            matches!(
                                c.classification,
                                Classification::ObservedOutsidePrediction { .. }
                            )
                        })
                        .map(|c| rustlitmus::evidence::describe(&c.classification))
                        .collect();
                    if b.lift_error.is_some() {
                        unsupported += 1;
                    }
                    use std::io::Write;
                    let sc = rustlitmus::score::score(&b);
                    writeln!(
                        index,
                        "{}",
                        serde_json::json!({"case": l.name, "config": cfg.label(), "divergence": b.localization.summary, "hardware_outside_prediction": hw_flags, "lift_error": b.lift_error.as_ref().map(|e| e.to_string()), "score": sc.total, "bucket": sc.bucket})
                    )?;
                    let mark = if !hw_flags.is_empty() {
                        "!!"
                    } else if needs_follow_up {
                        "~"
                    } else {
                        " "
                    };
                    println!("{mark} {:<40} {}", l.name, b.localization.summary);
                    for h in &hw_flags {
                        println!("     HARDWARE OUTSIDE PREDICTION: {h}");
                    }
                    if needs_follow_up || !hw_flags.is_empty() {
                        divergent.push((l.name.clone(), b.localization.summary.clone(), hw_flags));
                    }
                }
            }
            println!("\nsweep: {n} cases, {} requiring follow-up, {unsupported} not liftable, {:.0?} elapsed", divergent.len(), start.elapsed());
            println!("index: {}", index_path.display());
        }
        Cmd::Inspect { bundle } => {
            let s = std::fs::read_to_string(&bundle)?;
            let b = Bundle::from_json(&s).map_err(|e| anyhow!(e))?;
            print!("{}", summarise(&b));
            println!("replay commands:");
            for r in &b.replay {
                println!("  {r}");
            }
        }
        Cmd::Replay {
            bundle,
            out,
            tools,
            budget,
            skip,
            source_model,
        } => {
            let s = std::fs::read_to_string(&bundle)?;
            let b = Bundle::from_json(&s).map_err(|e| anyhow!(e))?;
            let cfg = b
                .config()
                .cloned()
                .ok_or_else(|| anyhow!("bundle has no compile config to replay"))?;
            let work = out.join(&b.litmus.name).join(cfg.label());
            let prov = Provenance {
                generator: "replay".into(),
                generation_reason: format!("replay of {}", b.case_id),
                seed: None,
                family: b.provenance.family.clone(),
                parent_case: Some(b.case_id.clone()),
                created_utc: now_utc(),
            };
            let source_model = source_model
                .or_else(|| b.herd_source.as_ref().map(|h| h.model.clone()))
                .unwrap_or_else(|| "models/rc11-p0982.cat".into());
            let tools = tools.tools();
            let budget = budget.budget();
            let stages = parse_skip(&skip);
            let context = RunContext {
                cfg: &cfg,
                tools: &tools,
                budget: &budget,
                stages: &stages,
                source_model: &source_model,
            };
            let nb = run_case(&b.litmus, context, &work, prov).map_err(|e| anyhow!(e))?;
            std::fs::write(work.join("bundle.json"), nb.to_json())?;
            print!("{}", summarise(&nb));
            let mut same = true;
            for l in &b.layers {
                let n = nb.layers.iter().find(|x| x.layer == l.layer);
                let eq = n.map(|x| x.outcomes == l.outcomes).unwrap_or(false);
                if !eq {
                    same = false;
                }
                println!(
                    "  {:<42} {}",
                    l.layer.name(),
                    if eq {
                        "outcome set reproduced"
                    } else {
                        "outcome set DIFFERS from preserved bundle"
                    }
                );
            }
            println!(
                "replay: {}",
                if same {
                    "all preserved outcome sets reproduced"
                } else {
                    "differences found (see above)"
                }
            );
            if !same {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_a_finite_sweep_case_cap() {
        assert!(validate_sweep_budget(0).is_err());
        assert!(validate_sweep_budget(1).is_ok());
    }
}
