//! Compiler artifact capture.
//!
//! For one rendered Rust program and one toolchain configuration, produce every
//! intermediate the pipeline exposes and extract, per thread function, the
//! sequence of *memory-model-relevant events* at each layer:
//!
//! * built MIR (`-Zdump-mir`, nightly only): atomic method calls as written;
//! * optimized MIR (`--emit=mir` / `runtime-optimized` dump): atomic intrinsic calls
//!   with resolved orderings;
//! * LLVM IR (`--emit=llvm-ir`, post-LLVM-optimisation at the requested opt level):
//!   `load atomic` / `store atomic` / `atomicrmw` / `cmpxchg` / `fence` with orderings;
//! * assembly (`--emit=asm`): the instruction stream of the thread function.
//!
//! Extraction is *syntactic* and deliberately narrow: it recognises only the shapes the
//! renderer produces. Anything unrecognised is preserved verbatim under `unparsed` so a
//! divergence in extraction is visible instead of silently dropped.

use crate::process::{run, RunSpec};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainId {
    /// The rustup toolchain name passed as `+name`, e.g. `stable`, `nightly`.
    pub name: String,
    pub rustc_version: String,
    pub commit_hash: Option<String>,
    pub llvm_version: Option<String>,
    pub host: Option<String>,
    pub release: Option<String>,
}

impl ToolchainId {
    pub fn is_nightly(&self) -> bool {
        self.release
            .as_deref()
            .is_some_and(|r| r.contains("nightly"))
            || self.name.starts_with("nightly")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileConfig {
    pub toolchain: String,
    pub target: String,
    pub opt_level: String,
    /// Extra `-C target-feature=...` / `-C target-cpu=...` style flags.
    pub extra_flags: Vec<String>,
}

impl CompileConfig {
    pub fn label(&self) -> String {
        let mut s = format!("{}-{}-O{}", self.toolchain, self.target, self.opt_level);
        for f in &self.extra_flags {
            // The linker path is host-specific plumbing, not a semantic dimension.
            if f.starts_with("-Clinker") {
                continue;
            }
            s.push('-');
            s.push_str(&f.replace(['=', ',', ' ', '/'], "_").replace("-C_", ""));
        }
        s
    }
}

/// One memory-model-relevant event extracted from a layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Load {
        loc: String,
        ord: String,
    },
    Store {
        loc: String,
        ord: String,
    },
    Rmw {
        loc: String,
        op: String,
        ord: String,
    },
    Cmpxchg {
        loc: String,
        success: String,
        failure: String,
    },
    Fence {
        ord: String,
    },
    /// Architecture-level instruction with a memory effect, kept as text.
    Asm {
        text: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerEvents {
    pub events: Vec<Event>,
    /// Lines that looked relevant but were not understood by the extractor.
    pub unparsed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadArtifacts {
    pub symbol: String,
    pub mir_built: Option<String>,
    pub mir_optimized: Option<String>,
    pub llvm_ir: Option<String>,
    pub asm: Option<String>,
    pub events_mir_built: Option<LayerEvents>,
    pub events_mir_optimized: Option<LayerEvents>,
    pub events_llvm_ir: Option<LayerEvents>,
    pub events_asm: Option<LayerEvents>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileResult {
    pub config: CompileConfig,
    pub toolchain: ToolchainId,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub stderr: String,
    pub binary: Option<PathBuf>,
    pub binary_sha256: Option<String>,
    pub threads: Vec<ThreadArtifacts>,
    /// Which optional layers were unavailable and why (e.g. built MIR needs nightly).
    pub unavailable: BTreeMap<String, String>,
}

pub fn rustc_path() -> PathBuf {
    crate::process::which("rustc").unwrap_or_else(|| PathBuf::from("rustc"))
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| format!("read current directory: {e}"))?;
    Ok(cwd.join(path))
}

/// Create a fresh artifact directory for one compiler invocation.
///
/// A failed `rustc` invocation can leave some outputs behind. Reusing a fixed directory
/// would let a later failed compile accidentally consume artifacts from an earlier
/// successful compile, so each attempt gets an isolated directory that is retained with
/// the evidence bundle.
fn fresh_attempt_dir(root: &Path) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("read system clock: {e}"))?
        .as_nanos();
    let pid = std::process::id();
    for suffix in 0..1000 {
        let dir = root.join(format!("attempt-{pid}-{nanos}-{suffix}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create {}: {e}", dir.display())),
        }
    }
    Err(format!(
        "could not allocate a fresh compile directory below {}",
        root.display()
    ))
}

pub fn toolchain_id(toolchain: &str) -> Result<ToolchainId, String> {
    let out = run(
        &RunSpec::new(rustc_path(), [&format!("+{toolchain}"), "-vV"])
            .timeout(Duration::from_secs(60)),
    )
    .map_err(|e| e.to_string())?;
    if out.exit_code != Some(0) {
        return Err(format!(
            "rustc +{toolchain} -vV failed: {}",
            out.stderr.trim()
        ));
    }
    let mut id = ToolchainId {
        name: toolchain.into(),
        rustc_version: String::new(),
        commit_hash: None,
        llvm_version: None,
        host: None,
        release: None,
    };
    for (i, line) in out.stdout.lines().enumerate() {
        if i == 0 {
            id.rustc_version = line.trim().into();
        }
        if let Some(v) = line.strip_prefix("commit-hash: ") {
            id.commit_hash = Some(v.trim().into());
        } else if let Some(v) = line.strip_prefix("LLVM version: ") {
            id.llvm_version = Some(v.trim().into());
        } else if let Some(v) = line.strip_prefix("host: ") {
            id.host = Some(v.trim().into());
        } else if let Some(v) = line.strip_prefix("release: ") {
            id.release = Some(v.trim().into());
        }
    }
    Ok(id)
}

/// Extract the function body for `symbol` from a MIR dump (`fn symbol(...) { ... }`).
pub fn extract_mir_fn(mir: &str, symbol: &str) -> Option<String> {
    let needle = format!("fn {symbol}(");
    let start = mir.find(&needle)?;
    let rest = &mir[start..];
    // Body ends at the first line that is exactly `}` at column 0.
    let mut end = rest.len();
    let mut off = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "}" {
            end = off + line.len();
            break;
        }
        off += line.len();
    }
    Some(rest[..end].to_string())
}

/// Extract `define ... @symbol(...) { ... }` from LLVM IR text.
pub fn extract_llvm_fn(ll: &str, symbol: &str) -> Option<String> {
    let needle = format!("@{symbol}(");
    let mut search = 0;
    while let Some(pos) = ll[search..].find(&needle) {
        let abs = search + pos;
        let line_start = ll[..abs].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if ll[line_start..].starts_with("define") {
            let rest = &ll[line_start..];
            let end = rest.find("\n}\n").map(|i| i + 3).unwrap_or(rest.len());
            return Some(rest[..end].to_string());
        }
        search = abs + needle.len();
    }
    None
}

/// Extract the assembly of `symbol` (from its label to the `.Lfunc_end` / next global).
pub fn extract_asm_fn(asm: &str, symbol: &str) -> Option<String> {
    let label = format!("{symbol}:");
    let mut body = Vec::new();
    let mut inside = false;
    for line in asm.lines() {
        if !inside {
            if line.trim_end() == label {
                inside = true;
                body.push(line.to_string());
            }
            continue;
        }
        if line.starts_with(".Lfunc_end") || line.trim_start().starts_with(".cfi_endproc") {
            break;
        }
        body.push(line.to_string());
    }
    inside.then(|| body.join("\n"))
}

fn ordering_from_mir(s: &str) -> Option<String> {
    for o in ["Relaxed", "Acquire", "Release", "AcqRel", "SeqCst"] {
        if s.contains(&format!("Ordering::{o}")) {
            return Some(
                o.to_lowercase()
                    .replace("acqrel", "acq_rel")
                    .replace("seqcst", "seq_cst"),
            );
        }
    }
    None
}

fn mir_block_label(line: &str) -> Option<usize> {
    let line = line.trim_start();
    let rest = line.strip_prefix("bb")?;
    let digits: String = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    let suffix = rest.get(digits.len()..)?.trim_start();
    if digits.is_empty() || !suffix.starts_with(':') {
        return None;
    }
    digits.parse().ok()
}

fn mir_block_target(fragment: &str) -> Option<usize> {
    let rest = fragment.trim_start().strip_prefix("bb")?;
    let digits: String = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

enum MirTerminator {
    Next(usize),
    Return,
}

fn mir_terminator(lines: &[&str]) -> Option<MirTerminator> {
    for line in lines.iter().rev() {
        let line = line.trim();
        if line == "return;" {
            return Some(MirTerminator::Return);
        }
        if let Some(target) = line.strip_prefix("goto ->").and_then(mir_block_target) {
            return Some(MirTerminator::Next(target));
        }
        if let Some(target) = line
            .split_once("return: ")
            .and_then(|(_, target)| mir_block_target(target))
        {
            return Some(MirTerminator::Next(target));
        }
        if line.contains("switchInt") || line.contains("otherwise:") {
            return None;
        }
    }
    None
}

/// Put lines from deterministic, straight-line MIR blocks in execution order.
///
/// `--emit=mir` orders basic blocks by their numeric label, not necessarily by the
/// successor relation. The renderer only emits straight-line thread bodies, so following
/// `return: bbN` and `goto -> bbN` prevents false event reordering. For control flow the
/// extractor does not understand, callers retain the original textual order instead.
fn mir_execution_lines(body: &str) -> Option<Vec<&str>> {
    let mut blocks: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    let mut current = None;
    for line in body.lines() {
        if let Some(block) = mir_block_label(line) {
            if blocks.insert(block, Vec::new()).is_some() {
                return None;
            }
            current = Some(block);
        } else if let Some(block) = current {
            blocks.get_mut(&block)?.push(line);
        }
    }

    let mut current = 0;
    let mut visited = BTreeSet::new();
    let mut ordered = Vec::new();
    loop {
        if !visited.insert(current) {
            return None;
        }
        let lines = blocks.get(&current)?;
        ordered.extend(lines.iter().copied());
        match mir_terminator(lines)? {
            MirTerminator::Next(next) => current = next,
            MirTerminator::Return => break,
        }
    }

    let unvisited_memory_block = blocks.iter().any(|(block, lines)| {
        !visited.contains(block)
            && lines
                .iter()
                .any(|line| line.contains("atomic") || line.contains("fence("))
    });
    (!unvisited_memory_block).then_some(ordered)
}

/// Events from *built* MIR: calls like `Atomic::<u32>::store(move _4, const 1_u32, move _5)`.
/// Orderings are not resolved at this stage (they are `move _5` locals), so we record
/// the method name and leave the ordering as `?` unless a constant is visible.
pub fn events_from_mir_built(body: &str) -> LayerEvents {
    let mut ev = LayerEvents::default();
    let lines = mir_execution_lines(body).unwrap_or_else(|| body.lines().collect());
    for line in lines {
        let l = line.trim();
        let is_fence = l.contains("atomic::fence") || l.contains("fence(");
        if !l.contains("Atomic::<u32>::") && !is_fence {
            continue;
        }
        let ord = ordering_from_mir(l).unwrap_or_else(|| "?".into());
        if l.contains("::store(") {
            ev.events.push(Event::Store {
                loc: "?".into(),
                ord,
            });
        } else if l.contains("::load(") {
            ev.events.push(Event::Load {
                loc: "?".into(),
                ord,
            });
        } else if l.contains("::swap(") {
            ev.events.push(Event::Rmw {
                loc: "?".into(),
                op: "xchg".into(),
                ord,
            });
        } else if l.contains("::fetch_add(") {
            ev.events.push(Event::Rmw {
                loc: "?".into(),
                op: "add".into(),
                ord,
            });
        } else if l.contains("::compare_exchange(") {
            ev.events.push(Event::Cmpxchg {
                loc: "?".into(),
                success: ord,
                failure: "?".into(),
            });
        } else if is_fence {
            ev.events.push(Event::Fence { ord });
        } else {
            ev.unparsed.push(l.to_string());
        }
    }
    ev
}

fn intrinsic_ordering(s: &str) -> Option<String> {
    // `AtomicOrdering::AcqRel` (intrinsic generic) or `Ordering::SeqCst` (const arg).
    for o in ["Relaxed", "Acquire", "Release", "AcqRel", "SeqCst"] {
        if s.contains(&format!("AtomicOrdering::{o}")) || s.contains(&format!("Ordering::{o}")) {
            return Some(
                o.to_lowercase()
                    .replace("acqrel", "acq_rel")
                    .replace("seqcst", "seq_cst"),
            );
        }
    }
    None
}

/// Locations in MIR appear as field projections `((*_1).N: ...)`; we key events by the
/// field index N, which equals the location index because `Locs` is `#[repr(C)]` with a
/// padding field after each atomic (so atomic `i` is field `2*i`).
fn mir_field_to_loc(body: &str, local: &str) -> Option<String> {
    // Find `local = &raw const (((*_1).N: ...` or `local = &raw mut (((*_1).N`.
    let pat = format!("{local} = &raw");
    let line = body.lines().find(|l| l.contains(&pat))?;
    let idx = line.find("(*_1).")? + "(*_1).".len();
    let digits: String = line[idx..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let field: usize = digits.parse().ok()?;
    Some(format!("loc{}", field / 2))
}

/// Events from optimized MIR, where atomics are intrinsic calls with resolved orderings.
pub fn events_from_mir_optimized(body: &str) -> LayerEvents {
    let mut ev = LayerEvents::default();
    let lines = mir_execution_lines(body).unwrap_or_else(|| body.lines().collect());
    for line in lines {
        let l = line.trim();
        let is_fence = l.contains("atomic_fence") || l.contains("fence(");
        let is_atomic = l.contains("atomic::atomic_")
            || l.contains("atomic_xadd")
            || l.contains("atomic_xchg")
            || l.contains("atomic_cxchg")
            || l.contains("atomic_compare_exchange")
            || is_fence
            || l.contains("atomic_store")
            || l.contains("atomic_load");
        if !is_atomic || !l.contains("-> [") {
            continue;
        }
        if is_fence {
            let ord = intrinsic_ordering(l).unwrap_or_else(|| "?".into());
            ev.events.push(Event::Fence { ord });
            continue;
        }
        // Pointer operand is the first `move _N` after `(`.
        let ptr_local = l.find('(').and_then(|i| {
            let args = &l[i + 1..];
            let a = args.trim_start_matches("move ").trim_start_matches("copy ");
            let name: String = a
                .chars()
                .take_while(|c| *c == '_' || c.is_ascii_digit())
                .collect();
            (!name.is_empty()).then_some(name)
        });
        let loc = ptr_local
            .as_deref()
            .and_then(|p| {
                // Follow one level of `_6 = copy _7 as *mut u32 (PtrToPtr)`.
                let cast = format!("{p} = copy ");
                let src = body.lines().find(|x| x.contains(&cast)).and_then(|x| {
                    let s = x.find("copy ")? + 5;
                    let name: String = x[s..]
                        .chars()
                        .take_while(|c| *c == '_' || c.is_ascii_digit())
                        .collect();
                    Some(name)
                });
                mir_field_to_loc(body, src.as_deref().unwrap_or(p))
            })
            .unwrap_or_else(|| "?".into());
        let ord = intrinsic_ordering(l).unwrap_or_else(|| "?".into());
        if l.contains("atomic_store") {
            ev.events.push(Event::Store { loc, ord });
        } else if l.contains("atomic_load") {
            ev.events.push(Event::Load { loc, ord });
        } else if l.contains("atomic_xadd") {
            ev.events.push(Event::Rmw {
                loc,
                op: "add".into(),
                ord,
            });
        } else if l.contains("atomic_xchg") {
            ev.events.push(Event::Rmw {
                loc,
                op: "xchg".into(),
                ord,
            });
        } else if l.contains("atomic_cxchg") || l.contains("atomic_compare_exchange") {
            // Intrinsic form: orderings as generics `AtomicOrdering::X, AtomicOrdering::Y`.
            // Library-wrapper form (`atomic_compare_exchange::<u32>(ptr, old, new, Ordering::S, Ordering::F)`):
            // orderings as const arguments.
            let mut ords = Vec::new();
            for marker in ["AtomicOrdering::", "atomic::Ordering::"] {
                let mut rest = l;
                while let Some(i) = rest.find(marker) {
                    let s = &rest[i + marker.len()..];
                    let name: String = s
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect();
                    ords.push(
                        name.to_lowercase()
                            .replace("acqrel", "acq_rel")
                            .replace("seqcst", "seq_cst"),
                    );
                    rest = &s[name.len()..];
                }
                if !ords.is_empty() {
                    break;
                }
            }
            let success = ords.first().cloned().unwrap_or_else(|| "?".into());
            let failure = ords.get(1).cloned().unwrap_or_else(|| "?".into());
            ev.events.push(Event::Cmpxchg {
                loc,
                success,
                failure,
            });
        } else {
            ev.unparsed.push(l.to_string());
        }
    }
    ev
}

/// Map an LLVM pointer operand back to a location index: `%locs` is location 0; a
/// `getelementptr inbounds nuw i8, ptr %locs, i64 N` is location `N / 64`.
fn llvm_ptr_to_loc(body: &str, ptr: &str) -> String {
    if ptr == "%locs" {
        return "loc0".into();
    }
    let def = format!("{ptr} = getelementptr");
    if let Some(line) = body.lines().find(|l| l.trim_start().starts_with(&def)) {
        if let Some(i) = line.rfind("i64 ") {
            let digits: String = line[i + 4..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<usize>() {
                return format!("loc{}", n / 64);
            }
        }
    }
    format!("?{ptr}")
}

fn llvm_order(tok: &str) -> Option<&'static str> {
    Some(match tok {
        "monotonic" => "relaxed",
        "acquire" => "acquire",
        "release" => "release",
        "acq_rel" => "acq_rel",
        "seq_cst" => "seq_cst",
        "unordered" => "unordered",
        _ => return None,
    })
}

/// Events from LLVM IR.
pub fn events_from_llvm(body: &str) -> LayerEvents {
    let mut ev = LayerEvents::default();
    for line in body.lines() {
        let l = line.trim();
        let toks: Vec<&str> = l
            .split_whitespace()
            .map(|t| t.trim_end_matches(','))
            .collect();
        if l.contains("load atomic") {
            // %0 = load atomic i32, ptr %locs seq_cst, align 4
            let ptr = toks
                .iter()
                .position(|t| *t == "ptr")
                .and_then(|i| toks.get(i + 1))
                .copied()
                .unwrap_or("?");
            let ord = toks.iter().find_map(|t| llvm_order(t)).unwrap_or("?");
            ev.events.push(Event::Load {
                loc: llvm_ptr_to_loc(body, ptr),
                ord: ord.into(),
            });
        } else if l.starts_with("store atomic") {
            // store atomic i32 1, ptr %_7 seq_cst, align 4
            let ptr = toks
                .iter()
                .position(|t| *t == "ptr")
                .and_then(|i| toks.get(i + 1))
                .copied()
                .unwrap_or("?");
            let ord = toks.iter().find_map(|t| llvm_order(t)).unwrap_or("?");
            ev.events.push(Event::Store {
                loc: llvm_ptr_to_loc(body, ptr),
                ord: ord.into(),
            });
        } else if l.contains("atomicrmw") {
            // %_4 = atomicrmw add ptr %_7, i32 1 acq_rel, align 4
            let i = toks.iter().position(|t| *t == "atomicrmw").unwrap_or(0);
            let op = toks.get(i + 1).copied().unwrap_or("?");
            let ptr = toks
                .iter()
                .position(|t| *t == "ptr")
                .and_then(|i| toks.get(i + 1))
                .copied()
                .unwrap_or("?");
            let ord = toks.iter().find_map(|t| llvm_order(t)).unwrap_or("?");
            ev.events.push(Event::Rmw {
                loc: llvm_ptr_to_loc(body, ptr),
                op: op.into(),
                ord: ord.into(),
            });
        } else if l.contains("cmpxchg") {
            // %0 = cmpxchg [weak] ptr %a, i32 0, i32 1 release acquire, align 4
            let ptr = toks
                .iter()
                .position(|t| *t == "ptr")
                .and_then(|i| toks.get(i + 1))
                .copied()
                .unwrap_or("?");
            let ords: Vec<&str> = toks.iter().filter_map(|t| llvm_order(t)).collect();
            ev.events.push(Event::Cmpxchg {
                loc: llvm_ptr_to_loc(body, ptr),
                success: ords.first().copied().unwrap_or("?").into(),
                failure: ords.get(1).copied().unwrap_or("?").into(),
            });
        } else if l.starts_with("fence") {
            let ord = toks.iter().find_map(|t| llvm_order(t)).unwrap_or("?");
            let scope = if l.contains("syncscope") {
                "singlethread "
            } else {
                ""
            };
            ev.events.push(Event::Fence {
                ord: format!("{scope}{ord}"),
            });
        } else if l.contains("atomic") && !l.starts_with(';') {
            ev.unparsed.push(l.to_string());
        }
    }
    ev
}

/// Events from assembly: every instruction that touches memory or orders it, in
/// program order, normalised to lower-case with the label/directive lines removed.
pub fn events_from_asm(body: &str, target: &str) -> LayerEvents {
    let mut ev = LayerEvents::default();
    for line in body.lines().skip(1) {
        let l = line.trim();
        if l.is_empty()
            || l.starts_with('.')
            || l.starts_with('#')
            || l.starts_with("//")
            || l.ends_with(':')
        {
            continue;
        }
        let lower = l.to_lowercase();
        let mnemonic = lower.split_whitespace().next().unwrap_or("");
        let mem_effect = if target.starts_with("aarch64") {
            l.contains('[')
                || mnemonic.starts_with("dmb")
                || mnemonic.starts_with("dsb")
                || mnemonic.starts_with("isb")
                || mnemonic.starts_with("bl")
                || mnemonic.starts_with("cas")
                || mnemonic.starts_with("swp")
                || mnemonic.starts_with("ld")
                || mnemonic.starts_with("st")
        } else {
            l.contains('(')
                || mnemonic.starts_with("lock")
                || mnemonic.contains("fence")
                || mnemonic.starts_with("xchg")
                || mnemonic.starts_with("call")
                || mnemonic.starts_with("cmpxchg")
        };
        if mem_effect
            || mnemonic.starts_with("ret")
            || mnemonic.starts_with('b')
            || mnemonic.starts_with('j')
            || mnemonic.starts_with("cb")
            || mnemonic.starts_with("tb")
        {
            ev.events.push(Event::Asm {
                text: lower.split_whitespace().collect::<Vec<_>>().join(" "),
            });
        }
    }
    ev
}

/// Compile `source` under `cfg`, writing artifacts into `out_dir`.
pub fn compile(
    source_path: &Path,
    out_dir: &Path,
    cfg: &CompileConfig,
    thread_symbols: &[String],
    timeout: Duration,
) -> Result<CompileResult, String> {
    // `rustc` runs in a fresh child of `out_dir` so its incidental files stay together
    // without being mistaken for artifacts from a prior attempt. Make input and output
    // paths absolute before constructing its command: otherwise a caller using relative
    // paths makes rustc look for `out_dir/source_path`.
    let source_path = absolute_path(source_path)?;
    let out_dir = absolute_path(out_dir)?;
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;
    let attempt_dir = fresh_attempt_dir(&out_dir)?;
    let tc = toolchain_id(&cfg.toolchain)?;
    let nightly = tc.is_nightly();
    let mir_dir = attempt_dir.join("mir-dumps");
    let out_base = attempt_dir.join("prog");
    let mut args: Vec<String> = vec![
        format!("+{}", cfg.toolchain),
        source_path.display().to_string(),
        "-o".into(),
        out_base.display().to_string(),
        "--target".into(),
        cfg.target.clone(),
        format!("-Copt-level={}", cfg.opt_level),
        "-Ccodegen-units=1".into(),
        "-Cdebuginfo=0".into(),
        "--emit=link,llvm-ir,asm,mir".into(),
        "-Cllvm-args=-x86-asm-syntax=att".into(),
    ];
    if nightly {
        // Nightly-only: per-pass MIR dumps give us *built* (pre-optimisation) MIR.
        args.push(format!("-Zdump-mir={}", thread_symbols.join("|")));
        args.push(format!("-Zdump-mir-dir={}", mir_dir.display()));
    }
    args.extend(cfg.extra_flags.iter().cloned());
    let spec = RunSpec::new(rustc_path(), args.iter().map(String::as_str))
        .timeout(timeout)
        .cwd(&attempt_dir);
    let command = spec.command_line();
    let out = run(&spec).map_err(|e| e.to_string())?;
    let succeeded = out.exit_code == Some(0);
    let mut unavailable = BTreeMap::new();
    if !succeeded {
        unavailable.insert(
            "compile".into(),
            format!(
                "rustc exited {:?}; compiler artifacts are not trusted",
                out.exit_code
            ),
        );
    }
    if !nightly {
        unavailable.insert(
            "mir_built".into(),
            format!(
                "toolchain {} is not nightly; -Zdump-mir unavailable",
                cfg.toolchain
            ),
        );
    }
    let read = |p: PathBuf| {
        succeeded
            .then(|| std::fs::read_to_string(&p).ok())
            .flatten()
    };
    let ll = read(out_base.with_extension("ll"));
    let asm = read(out_base.with_extension("s"));
    let mir = read(out_base.with_extension("mir"));
    if ll.is_none() {
        unavailable.insert("llvm_ir".into(), "rustc did not produce .ll".into());
    }
    if asm.is_none() {
        unavailable.insert("asm".into(), "rustc did not produce .s".into());
    }
    if mir.is_none() {
        unavailable.insert("mir_optimized".into(), "rustc did not produce .mir".into());
    }
    let binary = if succeeded && out_base.exists() {
        Some(out_base.clone())
    } else {
        None
    };
    let binary_sha256 = binary
        .as_ref()
        .and_then(|b| std::fs::read(b).ok())
        .map(|bytes| {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(bytes))
        });
    let mut threads = Vec::new();
    for sym in thread_symbols {
        let mir_built = if succeeded && nightly {
            std::fs::read_dir(&mir_dir)
                .ok()
                .and_then(|rd| {
                    rd.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                        let n = p.file_name().unwrap().to_string_lossy().into_owned();
                        n.contains(&format!(".{sym}.")) && n.ends_with("built.after.mir")
                    })
                })
                .and_then(|p| std::fs::read_to_string(p).ok())
        } else {
            None
        };
        let mir_optimized = mir.as_deref().and_then(|m| extract_mir_fn(m, sym));
        let llvm_ir = ll.as_deref().and_then(|m| extract_llvm_fn(m, sym));
        let asm_fn = asm.as_deref().and_then(|m| extract_asm_fn(m, sym));
        threads.push(ThreadArtifacts {
            symbol: sym.clone(),
            events_mir_built: mir_built.as_deref().map(events_from_mir_built),
            events_mir_optimized: mir_optimized.as_deref().map(events_from_mir_optimized),
            events_llvm_ir: llvm_ir.as_deref().map(events_from_llvm),
            events_asm: asm_fn.as_deref().map(|a| events_from_asm(a, &cfg.target)),
            mir_built,
            mir_optimized,
            llvm_ir,
            asm: asm_fn,
        });
    }
    Ok(CompileResult {
        config: cfg.clone(),
        toolchain: tc,
        command,
        exit_code: out.exit_code,
        stderr: out.stderr,
        binary,
        binary_sha256,
        threads,
        unavailable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LL: &str = r#"
define dso_local void @rl_thread_1(ptr nofree noundef nonnull align 4 captures(none) %locs, ptr noalias %regs) unnamed_addr #4 {
bb2:
  %_7 = getelementptr inbounds nuw i8, ptr %locs, i64 64
  store atomic i32 1, ptr %_7 seq_cst, align 4
  %0 = load atomic i32, ptr %locs seq_cst, align 4
  store i32 %0, ptr %regs, align 4
  %_4 = atomicrmw add ptr %_7, i32 1 acq_rel, align 4
  %1 = getelementptr inbounds nuw i8, ptr %regs, i64 4
  store i32 %_4, ptr %1, align 4
  %2 = cmpxchg ptr %locs, i32 0, i32 1 release acquire, align 4
  fence seq_cst
  fence syncscope("singlethread") release
  ret void
}

define void @other() {
  ret void
}
"#;

    #[test]
    fn extracts_llvm_events() {
        let f = extract_llvm_fn(LL, "rl_thread_1").unwrap();
        assert!(f.starts_with("define dso_local void @rl_thread_1"));
        assert!(f.trim_end().ends_with('}'));
        assert!(!f.contains("@other"));
        let ev = events_from_llvm(&f);
        assert_eq!(
            ev.events,
            vec![
                Event::Store {
                    loc: "loc1".into(),
                    ord: "seq_cst".into()
                },
                Event::Load {
                    loc: "loc0".into(),
                    ord: "seq_cst".into()
                },
                Event::Rmw {
                    loc: "loc1".into(),
                    op: "add".into(),
                    ord: "acq_rel".into()
                },
                Event::Cmpxchg {
                    loc: "loc0".into(),
                    success: "release".into(),
                    failure: "acquire".into()
                },
                Event::Fence {
                    ord: "seq_cst".into()
                },
                Event::Fence {
                    ord: "singlethread release".into()
                },
            ]
        );
        assert!(ev.unparsed.is_empty(), "{:?}", ev.unparsed);
    }

    const MIR_OPT: &str = r#"
fn rl_thread_1(_1: &Locs, _2: &mut [u32; 2]) -> () {
    bb0: {
        _7 = &raw const (((*_1).2: std::sync::atomic::Atomic<u32>).0: std::cell::UnsafeCell<std::sync::atomic::private::Align4<u32>>);
        _6 = copy _7 as *mut u32 (PtrToPtr);
        _5 = atomic::atomic_store::<u32, false>(move _6, const 1_u32, const std::sync::atomic::Ordering::SeqCst) -> [return: bb1, unwind terminate(abi)];
    }
    bb1: {
        _9 = &raw const (((*_1).0: std::sync::atomic::Atomic<u32>).0: std::cell::UnsafeCell<std::sync::atomic::private::Align4<u32>>);
        _8 = copy _9 as *const u32 (PtrToPtr);
        _3 = atomic::atomic_load::<u32, false>(move _8, const std::sync::atomic::Ordering::SeqCst) -> [return: bb2, unwind terminate(abi)];
    }
    bb2: {
        _11 = &raw const (((*_1).2: std::sync::atomic::Atomic<u32>).0: std::cell::UnsafeCell<std::sync::atomic::private::Align4<u32>>);
        _10 = copy _11 as *mut u32 (PtrToPtr);
        _4 = atomic_xadd::<u32, u32, std::intrinsics::AtomicOrdering::AcqRel>(move _10, const 1_u32) -> [return: bb3, unwind unreachable];
    }
}

fn main() -> () {
}
"#;

    #[test]
    fn extracts_mir_optimized_events() {
        let f = extract_mir_fn(MIR_OPT, "rl_thread_1").unwrap();
        assert!(!f.contains("fn main"));
        let ev = events_from_mir_optimized(&f);
        assert_eq!(
            ev.events,
            vec![
                Event::Store {
                    loc: "loc1".into(),
                    ord: "seq_cst".into()
                },
                Event::Load {
                    loc: "loc0".into(),
                    ord: "seq_cst".into()
                },
                Event::Rmw {
                    loc: "loc1".into(),
                    op: "add".into(),
                    ord: "acq_rel".into()
                },
            ]
        );
    }

    #[test]
    fn extracts_mir_fence_call() {
        let mir = r#"
fn rl_thread_0(_1: &Locs) -> () {
    bb0: {
        _3 = fence(const std::sync::atomic::Ordering::SeqCst) -> [return: bb1, unwind terminate(abi)];
    }
}
"#;
        let f = extract_mir_fn(mir, "rl_thread_0").unwrap();
        let expected = LayerEvents {
            events: vec![Event::Fence {
                ord: "seq_cst".into(),
            }],
            unparsed: Vec::new(),
        };
        assert_eq!(events_from_mir_built(&f), expected);
        assert_eq!(events_from_mir_optimized(&f), expected);
    }

    #[test]
    fn orders_mir_events_by_control_flow() {
        let mir = r#"
fn rl_thread_0(_1: &Locs) -> () {
    bb0: {
        _7 = &raw const (((*_1).0: std::sync::atomic::Atomic<u32>).0: std::cell::UnsafeCell<std::sync::atomic::private::Align4<u32>>);
        _6 = copy _7 as *mut u32 (PtrToPtr);
        _5 = atomic::atomic_store::<u32>(move _6, const 1_u32, const std::sync::atomic::Ordering::Relaxed) -> [return: bb2, unwind terminate(abi)];
    }
    bb1: {
        _9 = &raw const (((*_1).2: std::sync::atomic::Atomic<u32>).0: std::cell::UnsafeCell<std::sync::atomic::private::Align4<u32>>);
        _8 = copy _9 as *const u32 (PtrToPtr);
        _4 = atomic::atomic_load::<u32>(move _8, const std::sync::atomic::Ordering::Relaxed) -> [return: bb3, unwind terminate(abi)];
    }
    bb2: {
        _3 = fence(const std::sync::atomic::Ordering::SeqCst) -> [return: bb1, unwind terminate(abi)];
    }
    bb3: {
        return;
    }
}
"#;
        let f = extract_mir_fn(mir, "rl_thread_0").unwrap();
        let ev = events_from_mir_optimized(&f);
        assert_eq!(
            ev.events,
            vec![
                Event::Store {
                    loc: "loc0".into(),
                    ord: "relaxed".into(),
                },
                Event::Fence {
                    ord: "seq_cst".into(),
                },
                Event::Load {
                    loc: "loc1".into(),
                    ord: "relaxed".into(),
                },
            ]
        );
    }

    const ASM: &str = "\t.text\nrl_thread_1:\n\t.cfi_startproc\n\tmovl\t$1, %eax\n\txchgl\t%eax, 64(%rdi)\n\tmovl\t(%rdi), %ecx\n\tmovl\t%ecx, (%rsi)\n\tlock\t\txaddl\t%eax, 64(%rdi)\n\tretq\n.Lfunc_end1:\n\t.size\trl_thread_1, .Lfunc_end1-rl_thread_1\nother:\n\tretq\n";

    #[test]
    fn extracts_asm_events() {
        let f = extract_asm_fn(ASM, "rl_thread_1").unwrap();
        assert!(!f.contains("other:"));
        let ev = events_from_asm(&f, "x86_64-unknown-linux-gnu");
        let texts: Vec<&str> = ev
            .events
            .iter()
            .map(|e| match e {
                Event::Asm { text } => text.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "xchgl %eax, 64(%rdi)",
                "movl (%rdi), %ecx",
                "movl %ecx, (%rsi)",
                "lock xaddl %eax, 64(%rdi)",
                "retq"
            ]
        );
    }

    #[test]
    fn extracts_mir_cas_wrapper_form() {
        let mir = r#"
fn rl_thread_1(_1: &Locs, _2: &mut [u32; 2]) -> () {
    bb0: {
        _8 = &raw const (((*_1).2: std::sync::atomic::Atomic<u32>).0: std::cell::UnsafeCell<std::sync::atomic::private::Align4<u32>>);
        _7 = copy _8 as *mut u32 (PtrToPtr);
        _3 = std::sync::atomic::atomic_compare_exchange::<u32>(move _7, const 7_u32, const 9_u32, const std::sync::atomic::Ordering::Relaxed, const std::sync::atomic::Ordering::Acquire) -> [return: bb5, unwind terminate(abi)];
    }
}
"#;
        let f = extract_mir_fn(mir, "rl_thread_1").unwrap();
        let ev = events_from_mir_optimized(&f);
        assert_eq!(
            ev.events,
            vec![Event::Cmpxchg {
                loc: "loc1".into(),
                success: "relaxed".into(),
                failure: "acquire".into()
            }]
        );
    }

    #[test]
    fn missing_symbol_is_none() {
        assert!(extract_llvm_fn(LL, "nope").is_none());
        assert!(extract_asm_fn(ASM, "nope").is_none());
        assert!(extract_mir_fn(MIR_OPT, "nope").is_none());
    }

    #[test]
    fn config_label_is_filesystem_safe() {
        let c = CompileConfig {
            toolchain: "nightly".into(),
            target: "aarch64-unknown-linux-gnu".into(),
            opt_level: "3".into(),
            extra_flags: vec!["-Ctarget-feature=+lse,+rcpc".into()],
        };
        let l = c.label();
        assert!(!l.contains('=') && !l.contains(',') && !l.contains('/'));
    }

    #[test]
    fn compiles_relative_paths() {
        let root = tempfile::Builder::new()
            .prefix("rustlitmus-compile-")
            .tempdir_in(".")
            .unwrap();
        let source = root.path().join("case.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let out = root.path().join("build");
        let cwd = std::env::current_dir().unwrap();
        let source = source.strip_prefix(&cwd).unwrap();
        let out = out.strip_prefix(&cwd).unwrap();
        let target = toolchain_id("stable").unwrap().host.unwrap();
        let cfg = CompileConfig {
            toolchain: "stable".into(),
            target,
            opt_level: "0".into(),
            extra_flags: Vec::new(),
        };

        let result = compile(source, out, &cfg, &[], Duration::from_secs(60)).unwrap();
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr);
        assert!(result.binary.is_some_and(|path| path.is_file()));
    }

    #[test]
    fn failed_compile_does_not_reuse_prior_artifacts() {
        let root = tempfile::Builder::new()
            .prefix("rustlitmus-stale-artifacts-")
            .tempdir_in(".")
            .unwrap();
        let source = root.path().join("case.rs");
        let out = root.path().join("build");
        let target = toolchain_id("stable").unwrap().host.unwrap();
        let cfg = CompileConfig {
            toolchain: "stable".into(),
            target,
            opt_level: "0".into(),
            extra_flags: Vec::new(),
        };
        let symbols = vec!["rl_thread_0".into()];

        std::fs::write(
            &source,
            "#[no_mangle]\npub extern \"C\" fn rl_thread_0() {}\nfn main() {}\n",
        )
        .unwrap();
        let first = compile(&source, &out, &cfg, &symbols, Duration::from_secs(60)).unwrap();
        assert_eq!(first.exit_code, Some(0), "{}", first.stderr);
        assert!(first.binary.is_some());

        std::fs::write(&source, "fn main( {\n").unwrap();
        let second = compile(&source, &out, &cfg, &symbols, Duration::from_secs(60)).unwrap();
        assert_ne!(second.exit_code, Some(0));
        assert!(second.binary.is_none());
        assert!(second.binary_sha256.is_none());
        assert!(second.threads.iter().all(|t| t.mir_built.is_none()
            && t.mir_optimized.is_none()
            && t.llvm_ir.is_none()
            && t.asm.is_none()));
        assert!(second.unavailable.contains_key("compile"));
    }
}
