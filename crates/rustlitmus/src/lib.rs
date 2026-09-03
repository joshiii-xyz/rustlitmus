//! RustLitmus: cross-layer semantic evidence and localisation for concurrent Rust.
//!
//! Pipeline for one litmus-shaped program:
//!
//! ```text
//! abstract litmus ──► Rust source ──► rustc ──► MIR / opt MIR / LLVM IR / asm ──► binary
//!        │                                                          │              │
//!        ├─► C11 litmus ─► herd7 rc11.cat  (source model)           │              │
//!        ├─► Miri weak-memory (source emulator, sampled)            │              │
//!        ├─► Miri-GenMC       (source model checker, exhaustive)    │              │
//!        │                                     lifted asm ─► herd7 aarch64/x86tso  │
//!        │                                                     (arch model)        │
//!        └───────────────────────────── compare / localise ◄──── hardware run ◄────┘
//! ```

pub mod compile;
pub mod evidence;
pub mod families;
pub mod hardware;
pub mod herd;
pub mod lift;
pub mod litmus;
pub mod miri;
pub mod pipeline;
pub mod process;
pub mod render_c11;
pub mod render_rust;
pub mod score;
