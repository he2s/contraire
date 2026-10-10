//! Micro-benchmarks, run as ignored tests so they can reach private functions and need
//! no benchmark framework:
//!
//!     cargo test --release -- --ignored bench_ --test-threads=1 --nocapture
//!
//! Test builds install a counting allocator, so every benchmark also reports
//! allocations per operation. Hot paths should show 0.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

thread_local! {
    // `const` init: touching it never allocates, so the allocator can use it.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn count() {
    let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.with(Cell::get)
}

/// Time `f` (auto-calibrated to ~0.2 s) and print ns/op, allocations/op and,
/// when `bytes` > 0, throughput.
pub fn run(name: &str, bytes: usize, mut f: impl FnMut()) {
    f();
    let mut iters = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        if t.elapsed().as_millis() >= 50 {
            break;
        }
        iters *= 2;
    }
    iters *= 4;
    let a0 = allocs();
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = t.elapsed().as_nanos() as f64 / iters as f64;
    let per_op = (allocs() - a0) as f64 / iters as f64;
    let tput = if bytes > 0 { format!("{:>9.0} MB/s", bytes as f64 / ns * 1e3) } else { String::new() };
    println!("bench {name:<30} {ns:>11.0} ns/op {per_op:>8.2} allocs/op {tput}");
}
