//! `overhead` — the per-wrapped-call overhead benchmark (NFR2 / DX invariant 8:
//! ≤10µs). Measures `Engine::execute` on the deployed current-thread
//! `block_on` path across four cases (baseline / empty / cache-miss / cache-hit;
//! see [`support`]). The ≤10µs claim lives here, in the release-optimized `bench`
//! profile — `cargo bench -p keelrun-core --bench overhead`. The debug tripwire
//! that guards CI against pathological regressions is `tests/overhead_guard.rs`.
//!
//! `main` has two modes. By default it runs the full criterion statistical
//! benchmark (rich report + regression detection). With `KEEL_BENCH_EMIT_JSON`
//! set it instead writes a deterministic machine artifact — `{p50_ns per case}`,
//! sorted keys, no timestamps — to `$KEEL_BENCH_OUT` (default
//! `target/bench-overhead.json`) for CI upload. `scripts/bench-overhead.sh`
//! drives both. CI wiring itself is Task 17.

use std::collections::BTreeMap;

use criterion::Criterion;

mod support;

/// Register the four overhead cases in one criterion group. Case ids are chosen
/// so their natural (ASCII) sort matches the intended order.
fn bench_overhead(c: &mut Criterion) {
    let cases = support::Cases::new();
    let mut group = c.benchmark_group("overhead");
    group.bench_function("0_baseline", |b| b.iter(|| cases.baseline()));
    group.bench_function("a_empty", |b| b.iter(|| cases.empty()));
    group.bench_function("b_cache_miss", |b| b.iter(|| cases.miss()));
    group.bench_function("c_cache_hit", |b| b.iter(|| cases.hit()));
    group.bench_function("d_events", |b| b.iter(|| cases.events()));
    group.finish();
}

/// Write the deterministic `{case: p50_ns}` artifact. Keys come from a
/// `BTreeMap`, so serialization is sorted-key; there are no timestamps.
fn emit_json() {
    let cases = support::Cases::new();
    let mut p50: BTreeMap<&'static str, u64> = BTreeMap::new();
    p50.insert("0_baseline", support::median_ns(|| cases.baseline()));
    p50.insert("a_empty", support::median_ns(|| cases.empty()));
    p50.insert("b_cache_miss", support::median_ns(|| cases.miss()));
    p50.insert("c_cache_hit", support::median_ns(|| cases.hit()));
    p50.insert("d_events", support::median_ns(|| cases.events()));

    let json = serde_json::to_string_pretty(&p50).expect("p50 map serialization is infallible");
    let path =
        std::env::var("KEEL_BENCH_OUT").unwrap_or_else(|_| "target/bench-overhead.json".to_owned());
    std::fs::write(&path, format!("{json}\n")).expect("bench-overhead.json must be writable");
    eprintln!("bench-overhead: wrote {path}\n{json}");
}

fn main() {
    // Deterministic artifact mode: emit JSON and skip the statistical run. The
    // cargo-passed bench args (`--bench`, filters) are simply ignored here.
    if std::env::var_os("KEEL_BENCH_EMIT_JSON").is_some() {
        emit_json();
        return;
    }
    // Statistical mode: the manual equivalent of `criterion_main!` (so the
    // JSON branch above can pre-empt it).
    let mut criterion = Criterion::default().configure_from_args();
    bench_overhead(&mut criterion);
    criterion.final_summary();
}
