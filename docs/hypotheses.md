# Hypothesis registry

States: speculative · active · weakened · supported · strongly-supported · falsified · superseded · unresolved.

Every hypothesis names its falsification condition *before* the experiment runs.

---

## H1 — Cross-layer localisation is feasible for Rust litmus programs

**Statement.** For litmus-shaped Rust programs (≤4 threads, straight-line atomics), one can
automatically produce comparable outcome sets from (a) the C++20/RC11 source model, (b) Miri's
weak-memory emulator, (c) Miri-GenMC, (d) the architecture model on the *lifted compiled
assembly*, and (e) hardware, and compute the earliest adjacent boundary at which the set changes.

**Motivation.** Téléchat does (a)+(d) for C/C++; nobody does it for rustc, and nobody adds
(b)/(c) or hardware in the same chain.

**Falsification.** If the lifted assembly cannot be represented for the shapes rustc emits, or if
the layers cannot be put on a common outcome alphabet, localisation is impossible.

**Evidence.** 2026-09-03: `SB+sc-sc-sc-sc` and `SB+rlx-rlx-rlx-rlx` on x86-64 (stable 1.98.1 /
LLVM 22.1.8) produce all five layers; all adjacent boundaries consistent; hardware observed
the weak SB outcome `0:r0=0 1:r0=0` for the relaxed variant as predicted (source model,
GenMC, x86-TSO on lifted `movl`/`movl`). Bundles under `evidence/`.

**Status.** supported (methodology). Not itself a discovery.

---

## H2 — rustc's atomic mappings on x86-64 and AArch64 are outcome-set-preserving for the litmus families

**Statement.** For every ordering instance of every family in `families.rs`, the architecture
model on the lifted compiled assembly permits a subset of the outcomes permitted by the RC11
source model (i.e., the mapping is sound). Strict subsets (mapping stronger than the model) are
expected and benign.

**Falsification.** Any instance where `arch-model ⊄ source-model` (classification
`LaterLayerWeaker`) that survives (i) re-run in a fresh process, (ii) manual inspection of the
lifted test for lifter error, (iii) confirmation under the C11 model variants `c11_*.cat`.

**Status.** active — sweep in progress (MP first, then all families, x86-64 then AArch64 with
`-outline-atomics`, LSE, RCpc).

---

## H3 — Miri's weak-memory emulator and Miri-GenMC disagree on some litmus instances in a
way that localises to a documented model gap

**Statement.** There exist ordering instances where Miri (sampled, N seeds) exhibits an outcome
that GenMC's exhaustive RC11 exploration does not enumerate (Miri emulator unsound w.r.t. RC11:
candidate for miri#5104-class bug), or where GenMC enumerates an outcome Miri never produces
and hardware does (emulator incomplete).

**Falsification.** No such instance across all families after N≥8 seeds × 200 rounds.

**Status.** speculative; both tools now wired.

---

## H4 — The LLVM 23 [AA] never-escaping-local change is observable through safe Rust `&mut`
publication patterns (rust-visible instance of llvm-project#198811)

**Statement.** With rustc nightly 1.100 (LLVM 23.1.1), a plain store through a `&mut u32`
parameter is sunk below a `Release` store in the same function when the `&mut` pointee does
not escape and control flow makes the store partially redundant; stable (LLVM 22.1.8)
preserves the order. Whether this is a *bug* depends on whether a caller can legitimately
observe the pointee from another thread after synchronising on the release store (the open
question in llvm-project#198811, nikic vs efriedma).

**Evidence.** Reproduced 2026-09-03 for `fn nocap(b: &mut u32, a: &AtomicU32, v: u32, c: bool)`
on both AArch64 (`stlr` before `str`) and x86-64 (`movl (%rsi)` before `mov %eax,(%rdi)`);
disappears without the branch (`simple`), with a raw pointer, or with `Box` publication.
Hardware probe (x86-64, 1.2M rounds, TSO): no stale read observed — as expected, since x86
does not reorder two stores and the reordering is compile-time only in the *other* direction
(the plain store moves *after* the release store: on TSO the observer could see flag=1 and
data=0 only if the two stores are reordered *in the store buffer*, which TSO forbids... but
the compiled order already has the flag store first, so the observer *should* be able to
observe flag=1, data=0 on x86 if the observer's read follows the flag read). Probe showed
0/1.2M — needs analysis: the observer thread spins on `flag` then reads `data`; the producer
executes `movl $1,(%rsi)` (flag) then `mov %eax,(%rdi)` (data); a stale read requires the
observer's `data` load to execute between the two producer stores, a window of ~1 cycle.
Not observed in 1.2M trials ⇒ rate < 2.5e-6 (95%) — insufficient sampling, not evidence of
absence. AArch64 hardware unavailable here.

**Prior art.** llvm-project#198811 (C, `restrict`), rust-lang/rust#144351 (asm fence). The
Rust `&mut` = `noalias` angle is *not* in either issue as of 2026-06-03.

**Status.** **weakened → reclassified as a control (source-level UB under Rust's aliasing
models)**, 2026-09-03 evening.

**Falsification experiment performed.** `experiments/h4-noalias-release/miri_check.rs`
constructs the exact observer that would witness the reordering: thread A calls
`producer(&mut *cell, &flag, 1, true)`; thread B spins on `flag` with `Acquire` and then
performs an *atomic relaxed load* of the cell. Miri verdicts (observed, nightly 1.100
miri 0.1.0 2e2b193f8a):

| Aliasing model | Seeds | Verdict |
| --- | --- | --- |
| Stacked Borrows (default) | seed 0, 3; preemption 0.9 | ok |
| Stacked Borrows | `-Zmiri-many-seeds=0..16` | **UB at seed 15**: "not granting access to tag <7402> because that would remove [Unique for <7371>] which is strongly protected" — the observer's read while `producer`'s `&mut b` argument is still live (protected) |
| Tree Borrows | default seed | **UB**: "this foreign read access would cause the protected tag (currently Unique) to become Disabled; protected tags must never be Disabled" |
| Tree Borrows | seed 5 | ok (interleaving happened not to expose it) |

Both of Rust's candidate aliasing models classify the *only* observer that could witness the
LLVM 23 reordering as UB, because the observer reads the pointee while the `&mut` argument is
protected (function still executing). This matches nikic's reading of `noalias` for C
(`restrict`), transplanted to Rust: the release store inside the function cannot make the
`&mut`-protected memory legitimately observable *during* the function's execution. Once
`producer` returns, its `&mut` is dead and any later observation is fine — and by then both
stores have executed. The reordering is therefore **not a Rust-visible miscompilation**; it is
a behaviour change visible only to UB programs.

Consequences:
1. H4 becomes a **positive control** for the definedness layer: a program whose compiled
   code *looks* wrong at the assembly layer (release store before the protected payload
   store) but whose only witness is UB. The system must classify such a case as
   "source-level undefined behaviour", not "compiler bug", and it now does so via Miri
   aliasing checks.
2. The failure appears only under some schedules/seeds: single-seed Miri runs (the common
   practice) pass. `-Zmiri-many-seeds` was necessary. This is itself a small but concrete
   methodological result: *aliasing-model UB in concurrent code is schedule-dependent, and
   the default seed missed it under Stacked Borrows.*
3. The remaining open question (efriedma's) is whether LLVM's own semantics for `noalias`
   should say this; that is an LLVM LangRef issue, not a Rust bug. Rust's model (Stacked /
   Tree Borrows protectors) is already unambiguous here.

**Prior art check.** llvm-project#198811 (2026-05-20, open) is the C `restrict` instance
and discusses exactly this soundness argument; the Rust protector angle is not in the thread
as of 2026-06-03 (last comment). rust-lang/rust#144351 is a different mechanism (`asm!`).
Not novel as a *compiler behaviour*; novel only as a *documented Rust-side adjudication*.


---

## H5 — herd7 `rc11.cat` (RC11 release sequences) vs C++20 (P0982) release sequences differ on
`MP+relseq` instances, and Miri/GenMC implement the C++20 version

**Statement.** For `MP+relseq` where T0 does `W x=1; W_rel y=1; W_rlx y=2` and T1 does
`R_acq y → 2; R x`, RC11 (as coded in `rc11.cat`) treats the same-thread relaxed store as
continuing the release sequence, so `r0=2 ∧ r1=0` is forbidden; C++20 (P0982) drops the
same-thread rule so it is allowed. Miri's data-race detector explicitly implements P0982
(`store_relaxed` overwrites `sync_vector`). GenMC's RC11 checker — to be inspected.

**Falsification.** herd7 rc11 and Miri agree on the outcome set for these instances.

**Status.** active; a *source-model vs source-model* disagreement that the system should
surface as `LaterLayerWeaker(SourceModel → SourceModelChecker)` or as an emulator outcome
outside the herd prediction. This is a known spec change, so it is a **control**, not a
discovery — but it tests that the machinery detects model-version disagreement.
