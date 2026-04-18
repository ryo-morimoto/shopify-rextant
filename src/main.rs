#[tokio::main]
async fn main() -> anyhow::Result<()> {
    shopify_rextant::run().await
}
