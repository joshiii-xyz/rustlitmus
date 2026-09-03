//! Lift the compiled assembly of the thread functions into a `herd7`
//! architecture-level litmus test (AArch64 or X86_64), so that the architecture
//! memory model can be evaluated on *exactly the instruction sequence rustc emitted*.
//!
//! This is the method of Téléchat (Geeson & Smith, CGO 2024) for C/C++, applied to
//! rustc output. The lifter is a restricted, checked translator, not a general
//! disassembler-to-herd converter:
//!
//! * It accepts only the instruction shapes produced for the renderer's thread
//!   functions at the supported targets/features (see `docs/lifting.md`). Anything else
//!   is a hard [`LiftError::Unsupported`] so an unrepresentable case is never silently
//!   approximated.
//! * The calling convention is fixed by the renderer: `x0`/`%rdi` = `&Locs`,
//!   `x1`/`%rsi` = `&mut regs`. Location `i` lives at byte offset `64*i` from `x0`
//!   (`#[repr(C)]`, 60-byte padding). Register `r` lives at offset `4*r` from `x1`.
//! * Stores of results into `regs` become herd registers (`0:X5=…`), so the lifted test's
//!   final state is comparable one-to-one with the source-level outcome tuple.
//! * Address arithmetic (`add x8, x0, #64`, `64(%rdi)`) is resolved statically.
//! * LL/SC loops (`ldxr`/`stxr`/`cbnz` back-edge) are kept as loops: herd7's AArch64
//!   model supports them natively.
//! * Calls into the `__aarch64_*` outline-atomics helpers are **unsupported** (the helper
//!   body is runtime-dispatched); the pipeline compiles AArch64 with
//!   `-outline-atomics` so the atomic sequences are inline.
//! * herd7's X86_64 frontend is AT&T syntax (`movl (x),%eax`, `lock xaddl %eax,(x)`,
//!   `cmpxchgl (x),%ecx`); we emit exactly the forms its parser accepts.

use crate::compile::CompileConfig;
use crate::litmus::{Litmus, Outcome};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiftError {
    Unsupported { thread: usize, line: String, reason: String },
    NoAsm { thread: usize },
    Target(String),
}

impl std::fmt::Display for LiftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiftError::Unsupported { thread, line, reason } => write!(f, "thread {thread}: unsupported instruction {line:?}: {reason}"),
            LiftError::NoAsm { thread } => write!(f, "thread {thread}: no assembly captured"),
            LiftError::Target(t) => write!(f, "unsupported target for lifting: {t}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lifted {
    pub arch: String,
    pub litmus_text: String,
    /// For each thread, for each source register index, the herd register name that holds
    /// its final value (e.g. `X5`, `rax`)—used to decode herd states back into [`Outcome`]s.
    pub reg_map: Vec<Vec<String>>,
    /// Instructions that were folded away (address arithmetic, result write-backs). Kept
    /// for auditability.
    pub folded: Vec<String>,
}

#[derive(Debug, Clone)]
struct Line {
    label: Option<String>,
    mnem: String,
    ops: Vec<String>,
    raw: String,
}

const DOLLAR: char = '$';
const PERCENT: char = '%';
const HASH: char = '#';

fn parse_lines(asm: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for raw in asm.lines().skip(1) {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('.') || l.starts_with("//") || l.starts_with(HASH) || l.starts_with(';') {
            continue;
        }
        if let Some(lbl) = l.strip_suffix(':') {
            out.push(Line { label: Some(lbl.to_string()), mnem: String::new(), ops: vec![], raw: raw.to_string() });
            continue;
        }
        let (mnem, rest) = l.split_once(char::is_whitespace).unwrap_or((l, ""));
        // Strip trailing comments. AArch64 uses `//`; x86 AT&T uses `#` but AArch64 uses
        // `#` for immediates, so only strip `#` comments when the operands look like AT&T.
        let rest = rest.split("//").next().unwrap_or("");
        let rest = if rest.contains(DOLLAR) || rest.contains(PERCENT) { rest.split(HASH).next().unwrap_or("") } else { rest };
        let mut ops = Vec::new();
        let mut depth = 0;
        let mut cur = String::new();
        for c in rest.chars() {
            match c {
                '[' | '(' => {
                    depth += 1;
                    cur.push(c)
                }
                ']' | ')' => {
                    depth -= 1;
                    cur.push(c)
                }
                ',' if depth == 0 => {
                    ops.push(cur.trim().to_string());
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
        if !cur.trim().is_empty() {
            ops.push(cur.trim().to_string());
        }
        out.push(Line { label: None, mnem: mnem.to_lowercase(), ops, raw: raw.to_string() });
    }
    out
}

/// Symbolic value of an integer register during static address resolution.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    /// `&Locs + off`
    Locs(i64),
    /// `&regs + off`
    Regs(i64),
    Imm(i64),
    Unknown,
}

fn parse_imm(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches(HASH).trim_start_matches(DOLLAR);
    if let Some(h) = s.strip_prefix("0x") {
        i64::from_str_radix(h, 16).ok()
    } else if let Some(h) = s.strip_prefix("-0x") {
        i64::from_str_radix(h, 16).ok().map(|v| -v)
    } else {
        s.parse().ok()
    }
}

fn label_name(l: &str) -> String {
    l.trim_start_matches('.').replace('.', "_")
}

// ---------------------------------------------------------------- AArch64

fn a64_base_reg(r: &str) -> String {
    let r = r.trim().to_lowercase();
    if r == "wzr" || r == "xzr" {
        return "zr".into();
    }
    if let Some(n) = r.strip_prefix('w').or_else(|| r.strip_prefix('x')) {
        if n.chars().all(|c| c.is_ascii_digit()) {
            return format!("x{n}");
        }
    }
    r
}

/// Parse `[x8]`, `[x0, #64]` → (base register, offset).
fn a64_mem(op: &str) -> Option<(String, i64)> {
    let inner = op.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut parts = inner.split(',');
    let base = a64_base_reg(parts.next()?);
    let off = match parts.next() {
        Some(o) => parse_imm(o)?,
        None => 0,
    };
    Some((base, off))
}

struct A64Thread {
    lines: Vec<String>,
    outputs: BTreeMap<usize, String>,
    init: Vec<(String, String)>,
}

fn lift_aarch64_thread(t: usize, asm: &str, litmus: &Litmus) -> Result<(A64Thread, Vec<String>), LiftError> {
    let lines = parse_lines(asm);
    let mut vals: BTreeMap<String, Val> = BTreeMap::new();
    vals.insert("x0".into(), Val::Locs(0));
    vals.insert("x1".into(), Val::Regs(0));
    let mut out = Vec::new();
    let mut outputs: BTreeMap<usize, String> = BTreeMap::new();
    let mut init: Vec<(String, String)> = vec![("X0".into(), litmus.locations[0].clone())];
    let mut folded = Vec::new();
    let nlocs = litmus.locations.len();
    let unsupported = |line: &Line, reason: &str| LiftError::Unsupported { thread: t, line: line.raw.trim().to_string(), reason: reason.into() };

    // Resolve a memory operand to `[Xn]` where Xn is initialised to the location symbol.
    // Offset forms `[x0, #64]` are rewritten to a synthetic pointer register `x2k`.
    let resolve = |op: &str, vals: &BTreeMap<String, Val>, init: &mut Vec<(String, String)>, line: &Line| -> Result<String, LiftError> {
        let (base, off) = a64_mem(op).ok_or_else(|| unsupported(line, "unparsable memory operand"))?;
        match vals.get(&base) {
            Some(Val::Locs(b)) => {
                let addr = b + off;
                if addr < 0 || addr % 64 != 0 || ((addr / 64) as usize) >= nlocs {
                    return Err(unsupported(line, "address does not resolve to a location"));
                }
                let loc = (addr / 64) as usize;
                let reg = if off == 0 { base.to_uppercase() } else { format!("X{}", 20 + loc) };
                if !init.iter().any(|(r, _)| *r == reg) {
                    init.push((reg.clone(), litmus.locations[loc].clone()));
                }
                Ok(format!("[{reg}]"))
            }
            Some(Val::Regs(_)) => Err(unsupported(line, "unexpected access through regs pointer")),
            _ => Err(unsupported(line, "memory operand base has unknown value")),
        }
    };

    for line in &lines {
        if let Some(l) = &line.label {
            out.push(format!("{}:", label_name(l)));
            continue;
        }
        let ops = &line.ops;
        let up = |i: usize| ops[i].to_uppercase();
        match line.mnem.as_str() {
            "mov" | "movz" => {
                let dst = a64_base_reg(&ops[0]);
                if let Some(imm) = parse_imm(&ops[1]) {
                    vals.insert(dst, Val::Imm(imm));
                    out.push(format!("MOV {}, #{}", up(0), imm));
                } else {
                    let src = a64_base_reg(&ops[1]);
                    let v = if src == "zr" { Val::Imm(0) } else { vals.get(&src).cloned().unwrap_or(Val::Unknown) };
                    if let Val::Locs(b) = &v {
                        if b % 64 == 0 && ((*b / 64) as usize) < nlocs {
                            init.push((dst.to_uppercase(), litmus.locations[(*b / 64) as usize].clone()));
                            vals.insert(dst, v);
                            folded.push(line.raw.trim().to_string());
                            continue;
                        }
                    }
                    vals.insert(dst, v);
                    out.push(format!("MOV {}, {}", up(0), up(1)));
                }
            }
            "add" => {
                let dst = a64_base_reg(&ops[0]);
                let src = a64_base_reg(&ops[1]);
                let imm = parse_imm(&ops[2]);
                match (vals.get(&src).cloned(), imm) {
                    (Some(Val::Locs(b)), Some(i)) => {
                        let addr = b + i;
                        if addr % 64 == 0 && ((addr / 64) as usize) < nlocs {
                            vals.insert(dst.clone(), Val::Locs(addr));
                            init.push((dst.to_uppercase(), litmus.locations[(addr / 64) as usize].clone()));
                            folded.push(line.raw.trim().to_string());
                        } else {
                            return Err(unsupported(line, "pointer arithmetic does not hit a location"));
                        }
                    }
                    (Some(Val::Regs(b)), Some(i)) => {
                        vals.insert(dst, Val::Regs(b + i));
                        folded.push(line.raw.trim().to_string());
                    }
                    (Some(Val::Imm(b)), Some(i)) => {
                        vals.insert(dst, Val::Imm(b + i));
                        out.push(format!("ADD {}, {}, #{}", up(0), up(1), i));
                    }
                    (_, Some(i)) => {
                        vals.insert(dst, Val::Unknown);
                        out.push(format!("ADD {}, {}, #{}", up(0), up(1), i));
                    }
                    (_, None) => {
                        vals.insert(dst, Val::Unknown);
                        out.push(format!("ADD {}, {}, {}", up(0), up(1), up(2)));
                    }
                }
            }
            "sub" => {
                vals.insert(a64_base_reg(&ops[0]), Val::Unknown);
                if let Some(i) = parse_imm(&ops[2]) {
                    out.push(format!("SUB {}, {}, #{}", up(0), up(1), i));
                } else {
                    out.push(format!("SUB {}, {}, {}", up(0), up(1), up(2)));
                }
            }
            "ldr" | "ldar" | "ldapr" | "ldxr" | "ldaxr" => {
                let m = resolve(&ops[1], &vals, &mut init, line)?;
                vals.insert(a64_base_reg(&ops[0]), Val::Unknown);
                out.push(format!("{} {}, {}", line.mnem.to_uppercase(), up(0), m));
            }
            "str" | "stlr" => {
                let (base, off) = a64_mem(&ops[1]).ok_or_else(|| unsupported(line, "unparsable store operand"))?;
                if let Some(Val::Regs(b)) = vals.get(&base) {
                    let addr = b + off;
                    if addr % 4 != 0 {
                        return Err(unsupported(line, "misaligned regs store"));
                    }
                    let r = (addr / 4) as usize;
                    let src = a64_base_reg(&ops[0]);
                    if src == "zr" {
                        return Err(unsupported(line, "constant-folded result written to regs"));
                    }
                    // Snapshot into a dedicated register (X24+r) so later reuse of `src`
                    // cannot clobber the recorded result. MOV has no memory effect.
                    let snap = format!("X{}", 24 + r);
                    out.push(format!("MOV {snap}, {}", src.to_uppercase()));
                    if outputs.insert(r, snap).is_some() {
                        return Err(unsupported(line, "register written twice"));
                    }
                    folded.push(format!("{} (result write-back snapshotted)", line.raw.trim()));
                    continue;
                }
                let m = resolve(&ops[1], &vals, &mut init, line)?;
                out.push(format!("{} {}, {}", line.mnem.to_uppercase(), up(0), m));
            }
            "stp" => {
                let (base, off) = a64_mem(&ops[2]).ok_or_else(|| unsupported(line, "unparsable stp operand"))?;
                if let Some(Val::Regs(b)) = vals.get(&base) {
                    let r = ((b + off) / 4) as usize;
                    for (k, reg) in [(&ops[0], r), (&ops[1], r + 1)] {
                        let src = a64_base_reg(k);
                        if src == "zr" {
                            return Err(unsupported(line, "constant-folded result written to regs"));
                        }
                        let snap = format!("X{}", 24 + reg);
                        out.push(format!("MOV {snap}, {}", src.to_uppercase()));
                        if outputs.insert(reg, snap).is_some() {
                            return Err(unsupported(line, "register written twice"));
                        }
                    }
                    folded.push(format!("{} (result write-back snapshotted)", line.raw.trim()));
                    continue;
                }
                return Err(unsupported(line, "stp to shared memory"));
            }
            "stxr" | "stlxr" => {
                let m = resolve(&ops[2], &vals, &mut init, line)?;
                out.push(format!("{} {}, {}, {}", line.mnem.to_uppercase(), up(0), up(1), m));
            }
            "cas" | "casa" | "casl" | "casal" | "swp" | "swpa" | "swpl" | "swpal" | "ldadd" | "ldadda" | "ldaddl" | "ldaddal" => {
                let m = resolve(&ops[2], &vals, &mut init, line)?;
                vals.insert(a64_base_reg(&ops[1]), Val::Unknown);
                if line.mnem.starts_with("cas") {
                    vals.insert(a64_base_reg(&ops[0]), Val::Unknown);
                }
                out.push(format!("{} {}, {}, {}", line.mnem.to_uppercase(), up(0), up(1), m));
            }
            "stadd" | "staddl" => {
                let m = resolve(&ops[1], &vals, &mut init, line)?;
                out.push(format!("{} {}, {}", line.mnem.to_uppercase(), up(0), m));
            }
            "dmb" | "dsb" => out.push(format!("{} {}", line.mnem.to_uppercase(), up(0))),
            "cbnz" | "cbz" => out.push(format!("{} {}, {}", line.mnem.to_uppercase(), up(0), label_name(&ops[1]))),
            "b" => out.push(format!("B {}", label_name(&ops[0]))),
            "b.ne" | "b.eq" => out.push(format!("{} {}", line.mnem.to_uppercase(), label_name(&ops[0]))),
            "cmp" => {
                if let Some(i) = parse_imm(&ops[1]) {
                    out.push(format!("CMP {}, #{}", up(0), i));
                } else {
                    out.push(format!("CMP {}, {}", up(0), up(1)));
                }
            }
            "clrex" => out.push("CLREX".into()),
            "ret" => break,
            "bl" => return Err(unsupported(line, "call to out-of-line helper (compile with -outline-atomics)")),
            _ => return Err(unsupported(line, "mnemonic not in lifter whitelist")),
        }
    }
    for r in 0..litmus.threads[t].num_regs() {
        if !outputs.contains_key(&r) {
            return Err(LiftError::Unsupported { thread: t, line: String::new(), reason: format!("no write-back found for source register r{r}") });
        }
    }
    Ok((A64Thread { lines: out, outputs, init }, folded))
}

fn render_columns(threads: &[Vec<String>]) -> String {
    let mut s = String::new();
    let heads: Vec<String> = (0..threads.len()).map(|t| format!(" P{t} ")).collect();
    let _ = writeln!(s, "{};", heads.join("|"));
    let max = threads.iter().map(|t| t.len()).max().unwrap_or(0);
    for i in 0..max {
        let row: Vec<String> = threads.iter().map(|t| format!(" {} ", t.get(i).map(String::as_str).unwrap_or(""))).collect();
        let _ = writeln!(s, "{};", row.join("|"));
    }
    s
}

fn render_aarch64(litmus: &Litmus, threads: Vec<A64Thread>, folded: Vec<String>) -> Lifted {
    let mut s = String::new();
    let _ = writeln!(s, "AArch64 {}", crate::render_c11::sanitize(&litmus.name));
    let _ = writeln!(s, "(* lifted from rustc output by rustlitmus; digest {} *)", litmus.digest());
    let _ = writeln!(s, "{{");
    for l in &litmus.locations {
        let _ = writeln!(s, "uint32_t {l} = 0;");
    }
    for (t, th) in threads.iter().enumerate() {
        let mut seen = std::collections::BTreeSet::new();
        for (r, v) in &th.init {
            if seen.insert(r.clone()) {
                let _ = writeln!(s, "{t}:{r}={v};");
            }
        }
    }
    let _ = writeln!(s, "}}");
    s.push_str(&render_columns(&threads.iter().map(|t| t.lines.clone()).collect::<Vec<_>>()));
    let mut reg_map = Vec::new();
    let mut locs = Vec::new();
    for (t, th) in threads.iter().enumerate() {
        let mut m = Vec::new();
        for r in 0..litmus.threads[t].num_regs() {
            let hr = th.outputs[&r].clone();
            locs.push(format!("{t}:{hr}"));
            m.push(hr);
        }
        reg_map.push(m);
    }
    if !locs.is_empty() {
        let _ = writeln!(s, "locations [{};]", locs.join("; "));
    }
    let _ = writeln!(s, "exists (true)");
    Lifted { arch: "AArch64".into(), litmus_text: s, reg_map, folded }
}

// ---------------------------------------------------------------- X86_64

/// Canonical 64-bit register name (`eax`→`rax`, `r8d`→`r8`).
fn x86_reg(r: &str) -> String {
    let r = r.trim().trim_start_matches(PERCENT).to_lowercase();
    match r.as_str() {
        "eax" => "rax".into(),
        "ebx" => "rbx".into(),
        "ecx" => "rcx".into(),
        "edx" => "rdx".into(),
        "esi" => "rsi".into(),
        "edi" => "rdi".into(),
        "ebp" => "rbp".into(),
        "esp" => "rsp".into(),
        _ => {
            if let Some(n) = r.strip_prefix('r').and_then(|x| x.strip_suffix('d')) {
                if n.chars().all(|c| c.is_ascii_digit()) {
                    return format!("r{n}");
                }
            }
            r
        }
    }
}

/// AT&T 32-bit spelling for herd (herd accepts `%eax`-style operands).
fn att32(canon: &str) -> String {
    match canon {
        "rax" => "%eax".into(),
        "rbx" => "%ebx".into(),
        "rcx" => "%ecx".into(),
        "rdx" => "%edx".into(),
        "rsi" => "%esi".into(),
        "rdi" => "%edi".into(),
        other => format!("%{other}d"),
    }
}

/// Parse `(%rdi)`, `64(%rdi)`, `-64(%rsp)` → (canonical base, off).
fn x86_mem(op: &str) -> Option<(String, i64)> {
    let op = op.trim();
    let paren = op.find('(')?;
    let off = if paren == 0 { 0 } else { parse_imm(&op[..paren])? };
    let inner = op[paren + 1..].strip_suffix(')')?;
    if inner.contains(',') {
        return None;
    }
    Some((x86_reg(inner), off))
}

enum X86Loc {
    Shared(String),
    RegSlot(usize),
}

struct X86Thread {
    lines: Vec<String>,
    outputs: BTreeMap<usize, String>,
}

fn lift_x86_thread(t: usize, asm: &str, litmus: &Litmus) -> Result<(X86Thread, Vec<String>), LiftError> {
    let lines = parse_lines(asm);
    let mut vals: BTreeMap<String, Val> = BTreeMap::new();
    vals.insert("rdi".into(), Val::Locs(0));
    vals.insert("rsi".into(), Val::Regs(0));
    let mut out = Vec::new();
    let mut outputs: BTreeMap<usize, String> = BTreeMap::new();
    let mut folded = Vec::new();
    let nlocs = litmus.locations.len();
    let unsupported = |line: &Line, reason: &str| LiftError::Unsupported { thread: t, line: line.raw.trim().to_string(), reason: reason.into() };
    let classify = |op: &str, vals: &BTreeMap<String, Val>, line: &Line| -> Result<Option<X86Loc>, LiftError> {
        let Some((base, off)) = x86_mem(op) else { return Ok(None) };
        match vals.get(&base) {
            Some(Val::Locs(b)) => {
                let addr = b + off;
                if addr < 0 || addr % 64 != 0 || ((addr / 64) as usize) >= nlocs {
                    return Err(unsupported(line, "address does not resolve to a location"));
                }
                Ok(Some(X86Loc::Shared(litmus.locations[(addr / 64) as usize].clone())))
            }
            Some(Val::Regs(b)) => {
                let addr = b + off;
                if addr < 0 || addr % 4 != 0 {
                    return Err(unsupported(line, "misaligned regs access"));
                }
                Ok(Some(X86Loc::RegSlot((addr / 4) as usize)))
            }
            _ => {
                if base == "rsp" {
                    return Ok(Some(X86Loc::Shared("__stack".into())));
                }
                Err(unsupported(line, "memory operand with unknown base"))
            }
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        i += 1;
        if let Some(l) = &line.label {
            out.push(format!("{}:", label_name(l)));
            continue;
        }
        // Normalise `lock` prefix: it may be alone on a line or fused (`lock\t\txaddl\t%eax, 64(%rdi)`).
        let (mnem, ops, locked): (String, Vec<String>, bool) = if line.mnem == "lock" {
            if line.ops.is_empty() {
                let next = lines.get(i).ok_or_else(|| unsupported(line, "dangling lock prefix"))?;
                i += 1;
                (next.mnem.clone(), next.ops.clone(), true)
            } else {
                let first = line.ops[0].clone();
                let (m, a) = first.split_once(char::is_whitespace).ok_or_else(|| unsupported(line, "malformed lock prefix"))?;
                let mut o = vec![a.trim().to_string()];
                o.extend(line.ops[1..].iter().cloned());
                (m.to_lowercase(), o, true)
            }
        } else {
            (line.mnem.clone(), line.ops.clone(), false)
        };
        let lock = if locked { "lock " } else { "" };
        let suffix = if mnem.ends_with('q') && mnem != "cmpxchgq" { "q" } else { "l" };
        let base = mnem.trim_end_matches(['l', 'q', 'b', 'w']).to_string();
        let base = if mnem == "cmpxchgl" || mnem == "cmpxchgq" || mnem == "cmpxchg" { "cmpxchg".to_string() } else { base };
        match base.as_str() {
            "mov" => {
                let src = &ops[0];
                let dst = &ops[1];
                match (classify(src, &vals, line)?, classify(dst, &vals, line)?) {
                    (Some(X86Loc::Shared(loc)), None) => {
                        let d = x86_reg(dst);
                        vals.insert(d.clone(), Val::Unknown);
                        out.push(format!("movl ({loc}),{}", att32(&d)));
                    }
                    (None, Some(X86Loc::Shared(loc))) => {
                        if let Some(imm) = parse_imm(src) {
                            out.push(format!("movl {DOLLAR}{imm},({loc})"));
                        } else {
                            out.push(format!("movl {},({loc})", att32(&x86_reg(src))));
                        }
                    }
                    (None, Some(X86Loc::RegSlot(r))) => {
                        if parse_imm(src).is_some() {
                            return Err(unsupported(line, "constant-folded result written to regs"));
                        }
                        let s = x86_reg(src);
                        // The compiler may reuse the same physical register for several
                        // results (write-back to `regs` frees it). herd has no memory for
                        // `regs`, so snapshot the value into a fresh, otherwise-unused
                        // register with a register-to-register move (no memory effect).
                        let snap = format!("r{}", 8 + r);
                        if vals.contains_key(&snap) {
                            return Err(unsupported(line, "snapshot register already in use"));
                        }
                        vals.insert(snap.clone(), Val::Unknown);
                        out.push(format!("movl {},{}", att32(&s), att32(&snap)));
                        if outputs.insert(r, snap).is_some() {
                            return Err(unsupported(line, "register written twice"));
                        }
                        folded.push(format!("{} (result write-back snapshotted)", line.raw.trim()));
                    }
                    (None, None) => {
                        let d = x86_reg(dst);
                        if let Some(imm) = parse_imm(src) {
                            vals.insert(d.clone(), Val::Imm(imm));
                            out.push(format!("mov{suffix} {DOLLAR}{imm},{}", if suffix == "q" { format!("{PERCENT}{d}") } else { att32(&d) }));
                        } else {
                            let s = x86_reg(src);
                            let v = vals.get(&s).cloned().unwrap_or(Val::Unknown);
                            vals.insert(d.clone(), v);
                            out.push(format!("mov{suffix} {},{}", if suffix == "q" { format!("{PERCENT}{s}") } else { att32(&s) }, if suffix == "q" { format!("{PERCENT}{d}") } else { att32(&d) }));
                        }
                    }
                    (Some(X86Loc::RegSlot(_)), _) => return Err(unsupported(line, "read from regs")),
                    _ => return Err(unsupported(line, "memory-to-memory move")),
                }
            }
            "xchg" => {
                let (reg, mem) = if x86_mem(&ops[1]).is_some() { (&ops[0], &ops[1]) } else { (&ops[1], &ops[0]) };
                let Some(X86Loc::Shared(loc)) = classify(mem, &vals, line)? else { return Err(unsupported(line, "xchg not on a location")) };
                let r = x86_reg(reg);
                vals.insert(r.clone(), Val::Unknown);
                out.push(format!("xchgl {},({loc})", att32(&r)));
            }
            "xadd" => {
                let Some(X86Loc::Shared(loc)) = classify(&ops[1], &vals, line)? else { return Err(unsupported(line, "xadd not on a location")) };
                let r = x86_reg(&ops[0]);
                vals.insert(r.clone(), Val::Unknown);
                out.push(format!("{lock}xaddl {},({loc})", att32(&r)));
            }
            "cmpxchg" => {
                let Some(X86Loc::Shared(loc)) = classify(&ops[1], &vals, line)? else { return Err(unsupported(line, "cmpxchg not on a location")) };
                let r = x86_reg(&ops[0]);
                vals.insert("rax".into(), Val::Unknown);
                out.push(format!("{lock}cmpxchgl ({loc}),{}", att32(&r)));
            }
            "inc" | "dec" | "add" | "or" | "and" | "sub" => {
                let memop = ops.last().unwrap();
                match classify(memop, &vals, line)? {
                    Some(X86Loc::Shared(loc)) if loc == "__stack" => {
                        // `lock orl $0, -64(%rsp)`: LLVM's x86 SeqCst fence idiom.
                        if locked {
                            out.push("mfence".into());
                            folded.push(format!("{} (locked RMW on stack slot ⇒ modelled as mfence)", line.raw.trim()));
                        } else {
                            return Err(unsupported(line, "unlocked arithmetic on stack"));
                        }
                    }
                    Some(X86Loc::Shared(loc)) => {
                        if ops.len() == 1 {
                            out.push(format!("{lock}{base}l ({loc})"));
                        } else if let Some(imm) = parse_imm(&ops[0]) {
                            out.push(format!("{lock}{base}l {DOLLAR}{imm},({loc})"));
                        } else {
                            out.push(format!("{lock}{base}l {},({loc})", att32(&x86_reg(&ops[0]))));
                        }
                    }
                    Some(X86Loc::RegSlot(_)) => return Err(unsupported(line, "arithmetic on regs slot")),
                    None => {
                        let d = x86_reg(memop);
                        vals.insert(d.clone(), Val::Unknown);
                        if ops.len() == 1 {
                            out.push(format!("{base}l {}", att32(&d)));
                        } else if let Some(imm) = parse_imm(&ops[0]) {
                            out.push(format!("{base}l {DOLLAR}{imm},{}", att32(&d)));
                        } else {
                            out.push(format!("{base}l {},{}", att32(&x86_reg(&ops[0])), att32(&d)));
                        }
                    }
                }
            }
            "xor" => {
                let d = x86_reg(&ops[1]);
                if x86_reg(&ops[0]) == d {
                    vals.insert(d.clone(), Val::Imm(0));
                    out.push(format!("movl {DOLLAR}0,{}", att32(&d)));
                } else {
                    vals.insert(d.clone(), Val::Unknown);
                    out.push(format!("xorl {},{}", att32(&x86_reg(&ops[0])), att32(&d)));
                }
            }
            "mfence" => out.push("mfence".into()),
            "lfence" => out.push("lfence".into()),
            "sfence" => out.push("sfence".into()),
            "ret" => break,
            "call" => return Err(unsupported(line, "call")),
            _ => return Err(unsupported(line, "mnemonic not in lifter whitelist")),
        }
    }
    for r in 0..litmus.threads[t].num_regs() {
        if !outputs.contains_key(&r) {
            return Err(LiftError::Unsupported { thread: t, line: String::new(), reason: format!("no write-back found for source register r{r}") });
        }
    }
    Ok((X86Thread { lines: out, outputs }, folded))
}

fn render_x86(litmus: &Litmus, threads: Vec<X86Thread>, folded: Vec<String>) -> Lifted {
    let mut s = String::new();
    let _ = writeln!(s, "X86_64 {}", crate::render_c11::sanitize(&litmus.name));
    let _ = writeln!(s, "(* lifted from rustc output by rustlitmus; digest {} *)", litmus.digest());
    let _ = writeln!(s, "{{");
    for l in &litmus.locations {
        let _ = writeln!(s, "uint32_t {l} = 0;");
    }
    let _ = writeln!(s, "}}");
    s.push_str(&render_columns(&threads.iter().map(|t| t.lines.clone()).collect::<Vec<_>>()));
    let mut reg_map = Vec::new();
    let mut locs = Vec::new();
    for (t, th) in threads.iter().enumerate() {
        let mut m = Vec::new();
        for r in 0..litmus.threads[t].num_regs() {
            let hr = th.outputs[&r].clone();
            locs.push(format!("{t}:{hr}"));
            m.push(hr);
        }
        reg_map.push(m);
    }
    if !locs.is_empty() {
        let _ = writeln!(s, "locations [{};]", locs.join("; "));
    }
    let _ = writeln!(s, "exists (true)");
    Lifted { arch: "X86_64".into(), litmus_text: s, reg_map, folded }
}

/// Lift all threads. `asm_per_thread[t]` is the extracted assembly body of thread `t`.
pub fn lift(litmus: &Litmus, cfg: &CompileConfig, asm_per_thread: &[Option<String>]) -> Result<Lifted, LiftError> {
    let mut folded_all = Vec::new();
    if cfg.target.starts_with("aarch64") {
        let mut ths = Vec::new();
        for (t, asm) in asm_per_thread.iter().enumerate() {
            let asm = asm.as_deref().ok_or(LiftError::NoAsm { thread: t })?;
            let (th, folded) = lift_aarch64_thread(t, asm, litmus)?;
            folded_all.extend(folded.into_iter().map(|f| format!("P{t}: {f}")));
            ths.push(th);
        }
        Ok(render_aarch64(litmus, ths, folded_all))
    } else if cfg.target.starts_with("x86_64") {
        let mut ths = Vec::new();
        for (t, asm) in asm_per_thread.iter().enumerate() {
            let asm = asm.as_deref().ok_or(LiftError::NoAsm { thread: t })?;
            let (th, folded) = lift_x86_thread(t, asm, litmus)?;
            folded_all.extend(folded.into_iter().map(|f| format!("P{t}: {f}")));
            ths.push(th);
        }
        Ok(render_x86(litmus, ths, folded_all))
    } else {
        Err(LiftError::Target(cfg.target.clone()))
    }
}

/// Decode a herd7 state line from a *lifted* test (`0:X8=1; 1:rax=0;`) into a
/// source-level [`Outcome`] using `reg_map`.
pub fn decode_state(line: &str, reg_map: &[Vec<String>]) -> Result<Outcome, String> {
    let mut vals: BTreeMap<(usize, String), u32> = BTreeMap::new();
    for item in line.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (lhs, rhs) = item.split_once('=').ok_or_else(|| format!("bad item {item:?}"))?;
        let (t, r) = lhs.split_once(':').ok_or_else(|| format!("bad lhs {lhs:?}"))?;
        let t: usize = t.parse().map_err(|_| format!("bad thread {t:?}"))?;
        let v: u32 = rhs.trim().parse().map_err(|_| format!("bad value {rhs:?}"))?;
        vals.insert((t, r.trim().to_lowercase()), v);
    }
    let mut out = Vec::new();
    for (t, regs) in reg_map.iter().enumerate() {
        let mut row = Vec::new();
        for hr in regs {
            let v = vals.get(&(t, hr.to_lowercase())).ok_or_else(|| format!("herd state {line:?} lacks {t}:{hr}"))?;
            row.push(*v);
        }
        out.push(row);
    }
    Ok(Outcome(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litmus::{Instr, Ord, RmwKind, Thread};

    fn sb() -> Litmus {
        Litmus {
            name: "SB".into(),
            locations: vec!["x".into(), "y".into()],
            threads: vec![
                Thread { instrs: vec![Instr::Store { loc: 0, value: 1, ord: Ord::SeqCst }, Instr::Load { loc: 1, reg: 0, ord: Ord::SeqCst }] },
                Thread { instrs: vec![Instr::Store { loc: 1, value: 1, ord: Ord::SeqCst }, Instr::Load { loc: 0, reg: 0, ord: Ord::SeqCst }] },
            ],
        }
    }

    fn cfg(target: &str) -> CompileConfig {
        CompileConfig { toolchain: "stable".into(), target: target.into(), opt_level: "3".into(), extra_flags: vec![] }
    }

    #[test]
    fn lifts_x86_sb() {
        let t0 = "rl_thread_0:\n\tmovl\t$1, %eax\n\txchgl\t%eax, (%rdi)\n\tmovl\t64(%rdi), %eax\n\tmovl\t%eax, (%rsi)\n\tretq\n";
        let t1 = "rl_thread_1:\n\tmovl\t$1, %eax\n\txchgl\t%eax, 64(%rdi)\n\tmovl\t(%rdi), %eax\n\tmovl\t%eax, (%rsi)\n\tretq\n";
        let l = lift(&sb(), &cfg("x86_64-unknown-linux-gnu"), &[Some(t0.into()), Some(t1.into())]).unwrap();
        assert!(l.litmus_text.contains("xchgl %eax,(x)"), "{}", l.litmus_text);
        assert!(l.litmus_text.contains("movl (y),%eax"), "{}", l.litmus_text);
        assert!(l.litmus_text.contains("locations [0:r8; 1:r8;]"), "{}", l.litmus_text);
        assert_eq!(l.reg_map, vec![vec!["r8"], vec!["r8"]]);
        let o = decode_state("0:r8=0; 1:r8=1;", &l.reg_map).unwrap();
        assert_eq!(o, Outcome(vec![vec![0], vec![1]]));
    }

    #[test]
    fn lifts_aarch64_sb_with_offset_addressing() {
        let t0 = "rl_thread_0:\n\tmov\tw8, #1\n\tadd\tx9, x0, #64\n\tstlr\tw8, [x0]\n\tldar\tw8, [x9]\n\tstr\tw8, [x1]\n\tret\n";
        let t1 = "rl_thread_1:\n\tmov\tw8, #1\n\tstlr\tw8, [x0, #64]\n\tldar\tw8, [x0]\n\tstr\tw8, [x1]\n\tret\n";
        let l = lift(&sb(), &cfg("aarch64-unknown-linux-gnu"), &[Some(t0.into()), Some(t1.into())]).unwrap();
        assert!(l.litmus_text.contains("0:X0=x;"), "{}", l.litmus_text);
        assert!(l.litmus_text.contains("0:X9=y;"), "{}", l.litmus_text);
        assert!(l.litmus_text.contains("1:X21=y;"), "{}", l.litmus_text);
        assert!(l.litmus_text.contains("STLR W8, [X0]"));
        assert!(l.litmus_text.contains("LDAR W8, [X9]"));
        assert!(l.litmus_text.contains("STLR W8, [X21]"));
        assert!(l.litmus_text.contains("locations [0:X24; 1:X24;]"), "{}", l.litmus_text);
    }

    #[test]
    fn rejects_outline_atomics_call() {
        let t0 = "rl_thread_0:\n\tbl\t__aarch64_cas4_acq_rel\n\tret\n";
        let e = lift(&sb(), &cfg("aarch64-unknown-linux-gnu"), &[Some(t0.into()), Some(t0.into())]).unwrap_err();
        assert!(matches!(e, LiftError::Unsupported { .. }));
    }

    #[test]
    fn rejects_missing_writeback() {
        let t0 = "rl_thread_0:\n\tmovl\t$1, %eax\n\txchgl\t%eax, (%rdi)\n\tretq\n";
        assert!(lift(&sb(), &cfg("x86_64-unknown-linux-gnu"), &[Some(t0.into()), Some(t0.into())]).is_err());
    }

    #[test]
    fn lock_prefix_forms_and_fence_idiom() {
        let t = "f:\n\tlock\t\txaddl\t%eax, 64(%rdi)\n\tmovl\t%eax, (%rsi)\n\tlock\n\tincl\t(%rdi)\n\tlock\t\torl\t$0, -64(%rsp)\n\tretq\n";
        let l = Litmus {
            name: "t".into(),
            locations: vec!["x".into(), "y".into()],
            threads: vec![Thread { instrs: vec![Instr::Rmw { loc: 1, reg: 0, value: 1, ord: Ord::SeqCst, kind: RmwKind::FetchAdd }] }],
        };
        let out = lift(&l, &cfg("x86_64-unknown-linux-gnu"), &[Some(t.into())]).unwrap();
        assert!(out.litmus_text.contains("lock xaddl %eax,(y)"), "{}", out.litmus_text);
        assert!(out.litmus_text.contains("lock incl (x)"), "{}", out.litmus_text);
        assert!(out.litmus_text.contains("mfence"), "{}", out.litmus_text);
    }

    /// Regression: rustc reuses `%eax` for both loads of MP's reader; without snapshotting,
    /// both source registers mapped to `rax` and herd reported only the second load.
    #[test]
    fn register_reuse_across_writebacks_is_snapshotted() {
        let t1 = "rl_thread_1:\n\tmovl\t64(%rdi), %eax\n\tmovl\t%eax, (%rsi)\n\tmovl\t(%rdi), %eax\n\tmovl\t%eax, 4(%rsi)\n\tretq\n";
        let t0 = "rl_thread_0:\n\tmovl\t$1, (%rdi)\n\tmovl\t$1, 64(%rdi)\n\tretq\n";
        let mp = Litmus {
            name: "MP".into(),
            locations: vec!["x".into(), "y".into()],
            threads: vec![
                Thread { instrs: vec![Instr::Store { loc: 0, value: 1, ord: Ord::Relaxed }, Instr::Store { loc: 1, value: 1, ord: Ord::Relaxed }] },
                Thread { instrs: vec![Instr::Load { loc: 1, reg: 0, ord: Ord::Relaxed }, Instr::Load { loc: 0, reg: 1, ord: Ord::Relaxed }] },
            ],
        };
        let l = lift(&mp, &cfg("x86_64-unknown-linux-gnu"), &[Some(t0.into()), Some(t1.into())]).unwrap();
        assert_eq!(l.reg_map, vec![Vec::<String>::new(), vec!["r8".into(), "r9".into()]]);
        assert!(l.litmus_text.contains("locations [1:r8; 1:r9;]"), "{}", l.litmus_text);
        let o = decode_state("1:r8=1; 1:r9=0;", &l.reg_map).unwrap();
        assert_eq!(o, Outcome(vec![vec![], vec![1, 0]]));
        // AArch64 counterpart.
        let a1 = "rl_thread_1:\n\tldr\tw8, [x0, #64]\n\tstr\tw8, [x1]\n\tldr\tw8, [x0]\n\tstr\tw8, [x1, #4]\n\tret\n";
        let a0 = "rl_thread_0:\n\tmov\tw8, #1\n\tstr\tw8, [x0]\n\tstr\tw8, [x0, #64]\n\tret\n";
        let l = lift(&mp, &cfg("aarch64-unknown-linux-gnu"), &[Some(a0.into()), Some(a1.into())]).unwrap();
        assert_eq!(l.reg_map, vec![Vec::<String>::new(), vec!["X24".into(), "X25".into()]]);
    }

    #[test]
    fn x86_cmpxchg_operand_order() {
        let t = "f:\n\tmovl\t$1, %ecx\n\txorl\t%eax, %eax\n\tlock\t\tcmpxchgl\t%ecx, (%rdi)\n\tmovl\t%eax, (%rsi)\n\tretq\n";
        let l = Litmus {
            name: "t".into(),
            locations: vec!["x".into()],
            threads: vec![Thread { instrs: vec![Instr::Rmw { loc: 0, reg: 0, value: 1, ord: Ord::SeqCst, kind: RmwKind::CompareExchange { expected: 0, failure: Ord::SeqCst } }] }],
        };
        let out = lift(&l, &cfg("x86_64-unknown-linux-gnu"), &[Some(t.into())]).unwrap();
        assert!(out.litmus_text.contains("lock cmpxchgl (x),%ecx"), "{}", out.litmus_text);
        assert!(out.litmus_text.contains("movl $0,%eax"), "{}", out.litmus_text);
    }
}
