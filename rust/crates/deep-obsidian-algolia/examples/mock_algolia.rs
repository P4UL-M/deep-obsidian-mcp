//! Standalone mock Algolia server for the shared-wiki demo:
//! `cargo run -p deep-obsidian-algolia --example mock_algolia -- 9200`

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(9200);
    deep_obsidian_algolia::mock::serve_on(port).await
}
