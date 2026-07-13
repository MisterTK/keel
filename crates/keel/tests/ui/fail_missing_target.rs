// v1 scope: the target is explicit-only (crates/keel-macros/src/lib.rs doc
// comment explains why signature/host inference was deferred).

#[keel::wrap]
async fn fetch_order(id: u32) -> Result<u32, std::io::Error> {
    Ok(id)
}

fn main() {}
