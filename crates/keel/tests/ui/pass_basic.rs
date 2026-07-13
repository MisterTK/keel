// A valid `#[keel::wrap]` usage: free async fn, explicit target, `Result<T, E>`
// return with `T: Serialize + DeserializeOwned`, `E: std::error::Error`.

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Order {
    id: u32,
}

#[derive(Debug, thiserror::Error)]
#[error("upstream failed")]
struct UpstreamError;

#[keel::wrap(target = "orders-api", idempotent = true)]
async fn fetch_order(id: u32) -> Result<Order, UpstreamError> {
    Ok(Order { id })
}

fn main() {}
