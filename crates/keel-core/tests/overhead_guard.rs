//! Debug-CI tripwire for per-wrapped-call overhead. The real ≤10µs claim
//! (NFR2 / DX invariant 8) is proven by the release-profile `overhead` criterion
//! bench; this test runs in the unoptimized `test` profile, so it asserts only a
//! deliberately generous ceiling — a guard against pathological regressions, not
//! a precise gate (debug is easily 10–30× slower, and CI must never flake).
//!
//! It shares the exact four cases with the bench via the [`support`] module, and
//! prints the measured medians (visible with `cargo test -- --nocapture`).

#[path = "../benches/support.rs"]
mod support;

#[test]
fn overhead_stays_under_debug_ceiling() {
    // Generous: debug `Engine::execute` is single-digit-to-tens of µs; 250µs is
    // ~10–50× headroom, so this trips only on a real regression, never on noise.
    const CEILING_NS: u64 = 250_000;

    let cases = support::Cases::new();
    let baseline = support::median_ns(|| cases.baseline());
    let empty = support::median_ns(|| cases.empty());
    let miss = support::median_ns(|| cases.miss());
    let hit = support::median_ns(|| cases.hit());
    let events = support::median_ns(|| cases.events());

    println!(
        "overhead medians (debug profile, ns/call): \
         0_baseline={baseline} a_empty={empty} b_cache_miss={miss} c_cache_hit={hit} \
         d_events={events}"
    );

    // The baseline is the runtime floor, not Keel overhead — printed, not gated.
    for (case, ns) in [
        ("a_empty", empty),
        ("b_cache_miss", miss),
        ("c_cache_hit", hit),
        ("d_events", events),
    ] {
        assert!(
            ns < CEILING_NS,
            "{case} overhead {ns}ns exceeds the debug ceiling {CEILING_NS}ns"
        );
    }
}
