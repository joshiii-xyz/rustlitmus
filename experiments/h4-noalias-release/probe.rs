//! H4 probe: does LLVM 23's "[AA] No synchronization effects for never-escaping identified
//! local" (llvm-project PR #196923) reorder a plain store through a `&mut` parameter past a
//! `Release` store in the same function, in a *sound* Rust program, in a way another thread
//! can observe after synchronising on the release store?
//!
//! Shape (from llvm-project#198811, translated to Rust): `b: &mut u32` is `noalias`. The
//! function writes `*b` then publishes a flag with `Release`. The caller has legitimate access
//! to `*b`'s memory via a different path *after* the function returns — but the *observer*
//! thread synchronises with the release store *inside* the function and then reads the
//! memory. Under the Rust aliasing model, the reader's access races with the `&mut`
//! (it is a read of memory that is exclusively borrowed until `producer` returns) unless the
//! reader's read happens-after the release store *and* the `&mut` is considered dead at
//! that point. This is exactly the question nikic/efriedma disagree on for `restrict`.
//!
//! We test three variants that differ in what the *source* guarantees:
//!   A. `&mut u32` param (noalias)            — the disputed case
//!   B. `*mut u32` param (no noalias)          — control: must not reorder
//!   C. `&mut u32` but the pointee escapes first — control: must not reorder
//!
//! Observation harness: the observer spins on the flag (Acquire) and then reads the data
//! cell with a relaxed *atomic* load (so the read itself is not a data race in the
//! C++20 sense even if the writer's access were non-atomic — mixed atomic/non-atomic access
//! to the same location by different threads *is* a data race in Rust's model though, so
//! variant A is only "sound" if the `&mut` is dead by then; see docs).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::*};
use std::thread;

#[repr(C, align(64))]
struct Cell(UnsafeCell<u32>);
unsafe impl Sync for Cell {}

struct Shared { data: Cell, flag: AtomicU32, round: AtomicUsize, done: AtomicUsize }
unsafe impl Sync for Shared {}

#[inline(never)]
fn producer_a(b: &mut u32, a: &AtomicU32, v: u32, c: bool) {
    if c { *b = v; a.store(v, Release); } else { *b = v + 1; }
}

#[inline(never)]
unsafe fn producer_b(b: *mut u32, a: &AtomicU32, v: u32, c: bool) {
    if c { *b = v; a.store(v, Release); } else { *b = v + 1; }
}

fn main() {
    let variant = std::env::args().nth(1).unwrap_or_else(|| "a".into());
    let rounds: usize = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(100_000);
    let s = Shared { data: Cell(UnsafeCell::new(0)), flag: AtomicU32::new(0), round: AtomicUsize::new(0), done: AtomicUsize::new(0) };
    let mut stale = 0u64; let mut fresh = 0u64;
    thread::scope(|sc| {
        sc.spawn(|| {
            for r in 1..=rounds {
                while s.round.load(Acquire) != r { std::hint::spin_loop(); }
                let c = std::hint::black_box(true);
                match variant.as_str() {
                    "a" => producer_a(unsafe { &mut *s.data.0.get() }, &s.flag, 1, c),
                    "b" => unsafe { producer_b(s.data.0.get(), &s.flag, 1, c) },
                    _ => panic!("variant a|b"),
                }
                s.done.fetch_add(1, Release);
            }
        });
        for r in 1..=rounds {
            unsafe { *s.data.0.get() = 0; }
            s.flag.store(0, Relaxed);
            s.round.store(r, Release);
            while s.flag.load(Acquire) == 0 { std::hint::spin_loop(); }
            // Observer read: through an AtomicU32 view of the same cell (relaxed).
            let d = unsafe { (*(s.data.0.get() as *const AtomicU32)).load(Relaxed) };
            if d == 0 { stale += 1 } else { fresh += 1 }
            while s.done.load(Acquire) != r { std::hint::spin_loop(); }
        }
    });
    println!("variant={} rounds={} fresh={} stale={}", variant, rounds, fresh, stale);
}
