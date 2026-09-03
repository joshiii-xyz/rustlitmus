# Research log

Chronological. Each entry records what was done, what was observed, and what it changed.
"Observed" means seen in this sandbox on this date; everything else is labelled.

## 2026-09-03 — Environment, prior art, first vertical path

### Environment (observed)
- Host: x86_64 KVM guest under gVisor (`Linux 4.19.0-gvisor`), 2 vCPUs online, AMD family
  175 model 17 (model name reported as "unknown"; flags include avx512, no `rtm`). Root in a
  container. No `/dev/kvm`. `unshare --user --pid` works. This means **hardware observations
  are from a virtualised x86-64 TSO machine with 2 vCPUs**, which limits the observable
  weak behaviours (store buffering shows up readily; multi-copy-atomicity is guaranteed by
  the ISA).
- Toolchains installed: rustup 1.29.1; stable `rustc 1.98.1 (48a229cea 2026-09-01)`, LLVM
  22.1.8; nightly `rustc 1.100.0-nightly (2e2b193f8 2026-09-02)`, LLVM 23.1.1, with
  `miri`, `rust-src`, `llvm-tools`, `rustc-dev`. Targets added: `aarch64-unknown-linux-gnu`,
  `riscv64gc-unknown-linux-gnu`, `powerpc64le-unknown-linux-gnu` (std only, cross-compile).
- clang/LLVM 18 from apt (for `opt`/`llc` experiments), `qemu-user` 8.2 (user-mode emulation
  of foreign binaries — labelled *emulated*, never hardware), `bubblewrap`.
- herdtools7 **7.58** built from opam (OCaml 4.14.2) at `/opt/opam/herd/bin/{herd7,litmus7,diy7}`.
- Miri from the rustup component: weak-memory + data-race modes work via the `miri` driver
  with `--sysroot ~/.cache/miri`. Its GenMC mode is **not** compiled in ("GenMC is not supported
  on this target").
- **Miri-GenMC built from source** (`rust-lang/miri` master at `2e2b193f8`, GenMC commit
  `29b03a66` pinned by `genmc-sys/build.rs`) with `./miri build --features=genmc`; driver at
  `/opt/src/miri/target/debug/miri`. Verified on a single-round SB: enumerates 4 executions
  including the weak `0,0`.
- RustMC (Ollie-Pearce/rustmc) requires LLVM 21 + `nightly-2025-08-20`; not built. Its
  function (exhaustive LLVM-IR checking under GenMC) is covered from the *MIR* side by
  Miri-GenMC, which is the more precise Rust-semantics oracle. Recorded as a limitation:
  we have no exhaustive oracle *at the LLVM-IR level*.
- Loom: not used; it requires source rewriting to shim types and is imprecise for SeqCst
  (see prior-art review). Not on the critical path for localisation.

### Prior-art conclusions (see `docs/prior-art.md`)
- The closest existing method is Téléchat (Geeson & Smith, CGO 2024): compile C/C++ litmus
  tests, lift the assembly into herd, compare source model vs architecture model. It has
  not been applied to rustc, and it does not include operational emulators, MIR-level
  exhaustive checking, or hardware in the same chain.
- Known open items that overlap our search region and therefore serve as **controls**:
  rust#144351 (asm fence), llvm#198811 (noalias store sunk past release store, LLVM 23),
  miri#5104 (emulator produces C++20-forbidden SC outcome).

### Probes (observed, before the system existed)
1. **CAS failure ordering on AArch64** (`compare_exchange(_, _, Relaxed, SeqCst)`): LLVM IR
   keeps `monotonic seq_cst`; the outline-atomics helper chosen is `__aarch64_cas4_acq_rel`
   and with `-outline-atomics` the inline sequence is `ldaxr/stlxr` (acq_rel). The failure
   ordering is folded into the strongest of the two for both success and failure paths.
   This is a *stronger* mapping (sound). Note Miri-GenMC documents the same collapse.
2. **AtomicU128** not exposed on stable/nightly std for x86-64 (`integer_atomics` does not
   provide it); dropped.
3. **noalias store sinking (llvm#198811) reproduced from plain Rust** — see `docs/hypotheses.md`
   H4. `stable` (LLVM 22) emits `str; stlr`, `nightly` (LLVM 23) emits `stlr; str` for
   `fn nocap(b: &mut u32, a: &AtomicU32, v: u32, c: bool) { if c { *b = v; a.store(v, Release) } else { *b = v + 1 } }`.
   Also on x86-64 (`movl (%rsi)` before `mov %eax,(%rdi)`). A hardware probe (x86-64,
   3×400k + 3×300k rounds, volatile observer read after acquire on the flag) observed
   **0 stale reads**; that is a sampling bound (<2.5e-6 at 95%), not evidence of absence, and
   the observer design has a ~1-instruction window. AArch64 hardware would be the decisive
   test (`stlr` then `str` lets the `str` become visible after the flag).

### First vertical path (observed)
- `rustlitmus run SB+sc-sc-sc-sc` and `SB+rlx-rlx-rlx-rlx` (x86-64, stable, O3): all five
  layers produced comparable outcome sets; all adjacent boundaries consistent. Relaxed SB
  showed the weak `0:r0=0 1:r0=0` on hardware (observed), in GenMC (predicted), in RC11
  (predicted), and in x86-TSO on the lifted `movl`/`movl` (predicted). SeqCst SB compiles to
  `xchgl`+`movl`, and x86-TSO on that lifted code forbids the weak outcome, matching RC11.
- The GenMC stage needed a **separate single-execution rendering** (`render_single`): the
  full harness (barrier + histogram loop) made GenMC's exploration blow up (>5 min for SB);
  the thread functions under test are identical in both renderings.
- Lifter details that mattered: herd7's X86_64 frontend is **AT&T syntax** and requires
  `cmpxchgl (x),%ecx` (memory first) while `xchgl %eax,(x)`; the default X86_64 model is
  `x86tso-mixed.cat` (`x86tso.cat` is for the legacy `X86` arch). LLVM's SeqCst fence idiom
  `lock orl $0,-64(%rsp)` is modelled as `mfence` (recorded in `folded`).

### Next
- Sweep every ordering instance of every family on x86-64 (stable), then AArch64 with
  `-outline-atomics` (LL/SC), `+lse`, `+lse,+rcpc` under `aarch64.cat`, looking for
  `LaterLayerWeaker` at the source-model → arch-model boundary (H2) and for
  emulator/model-checker disagreements (H3, H5).
