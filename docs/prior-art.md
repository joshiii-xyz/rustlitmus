# Prior-art review

Status: living document. Every row lists what was actually inspected (source,
paper, docs, or issue tracker), not what is assumed. Dates are the dates the
material was read for this project.

## Tools and systems

| System | What it does | What it does not do (relevant here) | Inspected |
| --- | --- | --- | --- |
| **Rustlantis** (Wang & Jung, OOPSLA 2024) | Randomized differential testing of rustc. Generates *MIR* programs directly (custom MIR), runs them at several opt levels and under Miri, compares printed hashes. Found 22 rustc bugs. | Single-threaded only. No atomics, no threads, no memory-model reasoning, no hardware, no herd/LLVM-IR comparison. | Paper (DOI 10.1145/3689780), abstract, ETH page |
| **RustMC** (Pearce, Lange, O'Keeffe, 2025) | Extends GenMC to Rust by compiling crate tests to LLVM IR (`nightly-2025-08-20`, LLVM 21) and intercepting `std::thread` spawn/join. Exhaustive stateless model checking of *LLVM IR* under GenMC's models. | Reasons about **unoptimised** LLVM IR as the program semantics; does not compare against hardware, assembly, or an architecture model; does not localise where a discrepancy appears; requires a specific old nightly. | arXiv 2502.06293, GitHub README |
| **Miri** (weak-memory + data-race modes) | Interprets MIR; store-buffer weak memory emulation (Lidbury & Donaldson POPL'17), vector-clock race detector; C++20 release-sequence rules. **Randomised**, not exhaustive; `-Zmiri-seed`, `-Zmiri-many-seeds`. | Documented in-source limitations (`weak_memory.rs`): never produces C++11-forbidden behaviour, *cannot* produce all allowed behaviours (e.g. no load buffering; `hb ∪ rf ∪ mo` acyclic); SC fences over-approximated as global AcqRel RMW ("rules out legal behavior"); SC-fix handling incomplete (miri#2301 closed, **miri#5104 open, 2026-06**: emulator produces a C++20-forbidden SC outcome). Store buffer bounded at 128. | `src/concurrency/weak_memory.rs`, `data_race.rs` (master, 2026-09-03); issues #2301, #5104, #4572 |
| **Miri-GenMC** (`-Zmiri-genmc`, WIP) | Exhaustive DPOR exploration by driving GenMC from Miri. Requires building Miri from source with `--features=genmc`. | Not shipped in the rustup `miri` component (`fatal error: GenMC is not supported on this target`). Documented gaps: no `compare_exchange_weak`, **separate CAS failure ordering not supported (max of success/failure used)**, no mixed-size accesses, RNG/global-state hazards, OOTA excluded via `po ∪ rf` acyclicity, no Stacked/Tree Borrows. Correctness tracker miri#4572. | `doc/genmc.md`, `genmc-sys/build.rs`, issue #4572 |
| **GenMC** (Kokologiannakis, Vafeiadis et al.) | Stateless model checker for LLVM IR under RC11/IMM/etc., optimal DPOR. | Consumes LLVM IR, not Rust; per above, RC11-style OOTA exclusion. | Source tree at commit `29b03a66` (pinned by Miri) |
| **Loom** (tokio-rs) | Permutation-based exploration of Rust programs using shim types (`loom::sync::atomic`). C11-inspired model. | Requires source rewriting to loom types; does **not** model SeqCst precisely, cannot produce all C11 weak behaviours (per "Marrying Miri and GenMC" thesis, ETH). Cannot detect UB. | README, thesis abstract |
| **herdtools7 / herd7** (Alglave, Maranget) | Axiomatic simulator over `.cat` models: `rc11.cat`, `c11_*.cat`, `aarch64.cat`, `x86tso.cat`, `riscv.cat`, `ppc.cat`. Enumerates all allowed final states. `litmus7` runs assembly litmus tests on hardware; `diy7` generates tests from relation cycles. | Models **litmus-test-shaped** programs (small, straight-line + simple control). Architecture-level inputs are hand-written assembly, not compiler output. No Rust frontend. `rc11.cat` release-sequence definition (`rs = [W]; (sb & loc)?; [W & ...]; (rf; rmw)*`) is the *RC11* (C++17-style) definition, not the C++20/P0982 weakened one. | v7.58 installed from opam; `herd/libdir/rc11.cat` read |
| **LKMM litmus tests / klitmus7** | Linux-kernel memory model in `.cat`, kernel-module runner. | Kernel C, not Rust; different primitives. | Kernel docs |
| **Téléchat** (Geeson & Smith, CGO 2024) | Compiler testing under relaxed memory: compiles C/C++ litmus tests, lifts the *compiled assembly* back into a herd litmus test, compares source-model (C11) vs architecture-model (AArch64) allowed outcomes. Found LLVM `atomic_exchange`+fence bug on AArch64 (via updated herd model). Deployed at Arm. | **C/C++ only**; does not test rustc; does not involve MIR; does not attempt earliest-layer localisation within the compiler pipeline; not public-Rust-facing. Closest prior art to this project's method. | CGO 2024 abstract, arXiv 2310.12337, 2401.09474 |
| **atomic-mixer / Mix Testing** (Geeson et al., OOPSLA 2024) | Mixes different compilers' atomics mappings to find ABI-composition bugs. | C/C++ only. | Abstract |
| **cmmtest** (Morisset, Pawan, Zappa Nardelli, PLDI 2013) | Theory of sound C11 optimisations; detects miscompilation by comparing memory traces of compiled code. Found gcc write introductions. | C only, 2013-era gcc. | Abstract |
| **C11Tester / tsan11** | Race detectors/fuzzers for C/C++11 atomics with weak-memory emulation. C11Tester covers ARM-observable behaviours beyond tsan11. | C/C++; not Rust; not compiler testing. | ASPLOS'21 abstract |
| **Rust compiler fuzzing** (Rustlantis, rustsmith, tree-splicer, etc.) | Differential testing of rustc at various optimisation levels. | Sequential programs. | Surveyed |
| **MIR validation** (`-Zvalidate-mir`, MIR opt test suite) | Structural well-formedness and pass-level FileCheck tests. | Not a semantic check of atomics or concurrency. | rustc-dev-guide |
| **LLVM validation** (Alive2, LLVM lit tests) | Alive2: translation validation of LLVM-IR transformations (refinement), includes atomics/fence modelling limits. | Alive2 does not model weak memory concurrency; it treats atomics conservatively. | Alive2 docs (prior knowledge; see limitations) |

## Known open discrepancy classes (2025–2026) that anchor the search

These are already *reported* phenomena. They are controls, not novelty.

1. **rust-lang/rust#144351** (open, P-high, I-unsound): empty `asm!("")` does not act as
   a compiler fence in LLVM; store to a not-yet-escaped local before an `asm!` + relaxed
   `AtomicPtr` publish is eliminated. Layer: LLVM alias analysis / `asm` memory effects.
2. **llvm/llvm-project#198811** (open, `confirmed`, `miscompilation`, 2026-05-20): after
   [AA] PR #196923 ("No synchronization effects for never-escaping identified local",
   LLVM 23), a plain store through a `restrict`/`noalias` pointer is sunk *below* a
   release store in the same function. Maintainer position (nikic): correct because
   `noalias` forbids observation during the function without a synchronizes-with edge
   *after return*. Debate ongoing with efriedma. **Rust relevance**: every `&mut T`
   argument is `noalias`. This project reproduced the reordering with plain Rust
   (`fn nocap(b: &mut u32, a: &AtomicU32, v: u32, c: bool)`) on both AArch64 and x86-64
   with rustc nightly 1.100 / LLVM 23.1.1; stable 1.98 / LLVM 22 preserves order.
   See `docs/research-log.md` 2026-09-03.
3. **rust-lang/miri#5104** (open, 2026-06): Miri's store-buffer emulator exhibits an
   outcome forbidden by C++20 SC semantics (SC-fix). Layer: Miri weak-memory model.
4. **rust-lang/rust#62256**: `compiler_fence` (`fence syncscope("singlethread")`) emits
   real instructions on some backends.
5. NVPTX atomics (rust#136480, #146686): orderings collapsed. Target-specific, out of scope
   for x86-64/AArch64 hardware here.

## Explicit answers required by the project brief

- **What does Rustlantis already do?** Sequential MIR fuzzing of rustc; no concurrency.
- **What does RustMC already do?** Exhaustive model checking of Rust *via LLVM IR* under
  GenMC; no hardware/assembly/architecture-model comparison; no localisation.
- **What does Miri already do?** Random weak-memory emulation with documented
  incompleteness and known SC-fix unsoundness; exhaustive mode only via out-of-tree build.
- **What does Loom already do?** Permutation exploration with shim types; imprecise SeqCst.
- **What does GenMC already do?** Exhaustive LLVM-IR model checking (RC11/IMM).
- **What does herdtools7 already do?** Axiomatic simulation of hand-written litmus tests
  under language- and architecture-level `.cat` models; hardware litmus runner (`litmus7`).
- **What does compiler fuzzing already do?** Sequential differential testing.
- **What does hardware litmus testing already do?** Confirms architecture models against
  silicon for hand-written assembly tests (diy7/litmus7).
- **What does existing MIR validation do?** Structural checks, FileCheck pass tests.
- **What does existing LLVM validation do?** Alive2 refinement checking (sequential).
- **What does the Linux memory-model ecosystem do?** LKMM `.cat`, kernel litmus tests.
- **Téléchat (closest)** does model-based compiler testing for C/C++ by lifting compiled
  assembly into herd, but not for Rust and without intra-pipeline localisation.

## What RustLitmus does that these systems do not (as of the current evidence)

1. Takes *Rust* litmus-shaped programs and produces, for one case, aligned artifacts at
   every layer: source, MIR, optimized MIR, LLVM IR, optimized LLVM IR, assembly.
2. Lifts the *compiled assembly* of each thread into a herd7 architecture litmus test
   (AArch64 / x86-64) so the architecture model can be run on what rustc actually emitted
   (the Téléchat method, applied to rustc for the first time as far as our search shows).
3. Compares the C++20/RC11 source-model prediction (herd7 `rc11.cat` on a C translation),
   Miri's operational emulator, Miri-GenMC's exhaustive exploration, the
   architecture-model prediction on emitted assembly, and hardware observation, and reports
   the **earliest layer** at which the outcome set changes.
4. Preserves every disagreement with a rich classification instead of pass/fail.

Novelty claims are provisional and re-checked per finding (see `docs/hypotheses.md`).
