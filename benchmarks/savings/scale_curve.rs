//! Scale-curve diagnostic for the savings benchmark (§0.1).
//!
//! Drives the **real** lens code path at 1×, 10×, and 50× the committed
//! fixture size and prints how the savings move with scale, plus the
//! artifact-vs-real classification. The computation lives in
//! `benchmarks/common/savings.rs` so `bench_report` renders the same numbers
//! into `BENCHMARKS.md`.
//!
//!   cargo run --bin bench_scale_curve

#[path = "../common/savings.rs"]
#[allow(dead_code)] // this binary uses only the scale-curve entry points
mod savings;

fn main() {
    let rows = savings::compute_scale_curve();
    println!("# lens savings scale curve\n");
    print!("{}", savings::render_scale_curve_markdown(&rows));
}
