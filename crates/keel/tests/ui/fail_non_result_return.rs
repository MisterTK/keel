// v1 scope: the wrapped function must return `Result<T, E>` with explicit
// type arguments (so the macro can inspect them at expansion time).

#[keel::wrap(target = "orders-api")]
async fn fetch_order(id: u32) -> u32 {
    id
}

fn main() {}
