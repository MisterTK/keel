// v1 scope: only `async fn` is supported (the engine chain is driven on the
// caller's async runtime).

#[keel::wrap(target = "orders-api")]
fn fetch_order(id: u32) -> Result<u32, std::io::Error> {
    Ok(id)
}

fn main() {}
