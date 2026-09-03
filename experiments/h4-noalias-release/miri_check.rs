//! Is the H4 pattern UB under Rust's aliasing model? Run under Miri with Stacked Borrows and
//! with Tree Borrows. If Miri accepts every interleaving, the program is (as far as Miri's
//! models go) sound Rust, and the LLVM 23 reordering would be a Rust-visible miscompilation.
//!
//! The reader's access is an *atomic* relaxed load of the cell that `producer` wrote via
//! `&mut`. Both data-race and aliasing semantics are exercised.
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering::*};
use std::thread;

#[repr(C, align(64))]
struct Cell(UnsafeCell<u32>);
unsafe impl Sync for Cell {}

#[inline(never)]
fn producer(b: &mut u32, a: &AtomicU32, v: u32, c: bool) {
    if c { *b = v; a.store(v, Release); } else { *b = v + 1; }
}

fn main() {
    for _ in 0..50 {
        let data = Cell(UnsafeCell::new(0));
        let flag = AtomicU32::new(0);
        thread::scope(|sc| {
            sc.spawn(|| {
                // The &mut is created here and lives until producer returns.
                producer(unsafe { &mut *data.0.get() }, &flag, 1, std::hint::black_box(true));
            });
            sc.spawn(|| {
                while flag.load(Acquire) == 0 { std::hint::spin_loop(); }
                let d = unsafe { (*(data.0.get() as *const AtomicU32)).load(Relaxed) };
                assert_eq!(d, 1, "observer saw stale data after acquiring flag");
            });
        });
    }
    println!("ok");
}
