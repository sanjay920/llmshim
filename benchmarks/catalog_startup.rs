//! Startup cost of the resident model catalog. No API keys, no network.
//! cargo run --release --example catalog_startup
//! valgrind --tool=callgrind ./target/release/examples/catalog_startup
//! valgrind --tool=callgrind ./target/release/examples/catalog_startup -- --construction-only
//!
//! Building a handle and reading it are two different prices on purpose: the
//! 4.6 MB vendored snapshot is folded by the first `snapshot()`, so a process
//! that starts and never looks up a model never pays for it. That is the whole
//! contract, so the check is the ratio between the two, not a fixed budget —
//! it stays meaningful on a machine slower or faster than the one it was
//! written on. Measured on x86_64 release: 0.019 ms against 116 ms (≈6100x),
//! and 332,096 instructions against 812,583,551 under callgrind.
use llmshim_catalog::{CatalogHandle, CatalogOptions};
use std::time::Instant;

/// A fold at construction would land within a small factor of the first read
/// rather than orders of magnitude above it.
const MIN_RATIO: f64 = 10.0;

fn millis(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let construction_only = std::env::args().any(|arg| arg == "--construction-only");

    let start = Instant::now();
    let handle = CatalogHandle::load(CatalogOptions::default())
        .expect("the vendored catalog is valid and local policy is readable");
    let construction = millis(start);
    println!(
        "llmshim {} · handle construction: {construction:.3} ms",
        env!("CARGO_PKG_VERSION")
    );
    if construction_only {
        return;
    }

    let start = Instant::now();
    let snapshot = handle.snapshot();
    let first_read = millis(start);
    let models = snapshot.models().count();

    println!("  first snapshot read: {first_read:.3} ms · {models} models");
    println!(
        "  ratio: {:.0}x",
        first_read / construction.max(f64::MIN_POSITIVE)
    );

    if construction * MIN_RATIO > first_read {
        panic!(
            "handle construction ({construction:.3} ms) is not {MIN_RATIO}x cheaper than the \
             first read ({first_read:.3} ms): the catalog is being folded before a model is looked up"
        );
    }
}
