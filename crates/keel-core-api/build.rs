//! Guards the crate-local vendored contract copy against drift.
//!
//! The single source of truth stays `contracts/core_api.rs` at the repo root;
//! `src/lib.rs` includes the crate-local copy in `contract/` so the crate is
//! self-contained when packaged (`cargo package` cannot ship files outside the
//! crate directory). In a workspace checkout both files exist and every build
//! asserts they are byte-identical; in a packaged crate the repo copy is
//! absent and the check is skipped. Refresh with `scripts/sync-vendored.sh`.

use std::fs;
use std::path::Path;

const VENDORED: &[(&str, &str)] = &[("../../contracts/core_api.rs", "contract/core_api.rs")];

fn main() {
    for &(source, vendored) in VENDORED {
        println!("cargo::rerun-if-changed={vendored}");
        println!("cargo::rerun-if-changed={source}");
        let source = Path::new(source);
        if !source.exists() {
            continue; // packaged crate: no repo checkout around us
        }
        let want = fs::read(source).expect("read repo contract file");
        let got = fs::read(vendored).expect("read vendored contract copy");
        assert!(
            want == got,
            "vendored contract copy {vendored} is stale against {}; \
             run scripts/sync-vendored.sh to refresh it",
            source.display()
        );
    }
}
