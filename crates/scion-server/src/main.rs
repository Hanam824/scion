use anyhow::Result;
use clap::Parser;
use scion_repo::Repo;
use std::path::PathBuf;
use std::sync::Arc;

/// Authoritative Scion server: history, commits, merges, rebases, blame.
#[derive(Parser)]
#[command(name = "scion-server", version)]
struct Args {
    /// Directory holding meta.sqlite and blobs/.
    #[arg(long, default_value = "./scion-data", env = "SCION_DATA")]
    data: PathBuf,
    #[arg(long, default_value = "127.0.0.1:7777", env = "SCION_LISTEN")]
    listen: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let repo = Arc::new(Repo::open_local(&args.data)?);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    eprintln!(
        "scion-server listening on http://{} (data: {})",
        listener.local_addr()?,
        args.data.display()
    );
    scion_server::serve(repo, listener).await
}
