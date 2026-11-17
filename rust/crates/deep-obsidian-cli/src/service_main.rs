#[tokio::main]
async fn main() {
    if let Err(error) = deep_obsidian_cli::service_entry::run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
