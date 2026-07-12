//! Build script for the napi addon. `napi_build::setup()` emits the
//! platform-specific link arguments a Node native addon requires — on macOS the
//! critical `-undefined dynamic_lookup`, which lets the cdylib link with the
//! `napi_*` N-API symbols left unresolved (they are provided by the Node host at
//! load time). This is what makes `cargo build -p keel-node` produce a loadable
//! `.node` without a Node library to link against.
fn main() {
    napi_build::setup();
}
