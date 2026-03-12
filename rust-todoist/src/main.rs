#[tokio::main]
async fn main() {
    if let Err(error) = symphony_rust_todoist::cli::run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
