# RustLitmus

RustLitmus is experimental research tooling for checking small concurrent Rust programs
across the layers that can disagree:

- a C11/C++20-style source-model rendering for herd7;
- rustc artifacts, including optimized MIR, LLVM IR, and assembly;
- a lifted architecture litmus test for herd7;
- optional Miri weak-memory and Miri-GenMC runs; and
- bounded native hardware sampling.

It writes one evidence bundle per run. The bundle records the generated programs, compiler
configuration, tool versions, command lines, extracted events, outcome sets, limitations,
and the earliest comparison that disagrees. A finding is a lead for reproduction, not proof
of a compiler bug.

## Status

The repository is an active prototype. The historical observations in
[`docs/research-log.md`](docs/research-log.md) were produced in another environment and are
not yet independently replayed here. No confirmed compiler defect is claimed by this
repository.

## Quick start

The basic build and unit suite need a current Rust toolchain with the `stable` channel
installed:

```bash
cargo test --workspace --all-targets --locked
cargo run -- families
```

This bounded native smoke run exercises rendering, compilation, artifact extraction, and
hardware sampling without claiming a source-model comparison:

```bash
cargo run -- run 'SB+rlx-rlx-rlx-rlx' \
  --out out/smoke \
  --hw-batches 1 \
  --hw-iters 1000 \
  --timeout-secs 30 \
  --skip herd-source,herd-arch,miri-weak,miri-genmc
```

The output bundle is written below `out/`. Inspect it with:

```bash
cargo run -- inspect out/smoke/SB+rlx-rlx-rlx-rlx/stable-x86_64-unknown-linux-gnu-O3/bundle.json
```

For a source-model and architecture-model run, provide a compatible `herd7` binary. Miri,
Miri-GenMC, and a foreign-target emulator are optional explicit inputs. Missing tools remain
visible as limitations in the bundle; they are never treated as agreement.

Sweeps default to 32 cases and require a positive `--max-cases` value. Increase that value
deliberately for a larger campaign. Every external tool invocation is bounded by
`--timeout-secs`; `--max-secs` is a secondary stop condition checked between cases.
The sweep's follow-up count excludes expected stronger mappings and unavailable layers.

## Evidence rules

- Native hardware outcomes are finite samples. Seeing an outcome is evidence that it
  occurred on that machine and configuration. Not seeing one is not evidence of impossibility.
- A source-model, architecture-model, and hardware result only become comparable when they
  describe the same rendered case and compiled artifact.
- Each compiler invocation receives a fresh artifact directory. A failed compilation has no
  trusted compiler artifacts or executable, so it cannot reuse a prior successful binary.
- Compiler-pipeline comparisons include full atomic event shapes, not only memory orderings.
- The bundled no-thin-air variant is diagnostic only. It is not a Rust, C++20, or hardware
  specification and cannot dismiss a possible compiler-mapping issue.
- Reproduce a candidate with preserved bundle inputs, another toolchain or target, and an
  independent oracle before reporting it externally.

## Research notes

- [`docs/prior-art.md`](docs/prior-art.md) records the research boundary and related tools.
- [`docs/hypotheses.md`](docs/hypotheses.md) records falsifiable hypotheses and controls.
- [`docs/research-log.md`](docs/research-log.md) records historical work and its verification
  status.
