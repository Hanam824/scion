use anyhow::Result;
use clap::Parser;
use scion_import::{import, ImportOptions};
use std::path::PathBuf;

/// Import a git repository's history into a Scion server.
#[derive(Parser)]
#[command(name = "scion-import", version)]
struct Args {
    /// Path to the git repository.
    git_dir: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:7777")]
    server: String,
    /// Git revision to import.
    #[arg(long, default_value = "HEAD")]
    branch: String,
    /// Scion ref to set to the imported tip.
    #[arg(long = "ref", default_value = "main")]
    target_ref: String,
    /// Mapping file for resuming (git sha -> changeset id).
    #[arg(long, default_value = "scion-import-state.json")]
    state: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    let opts = ImportOptions {
        git_dir: a.git_dir,
        server: a.server,
        branch: a.branch,
        target_ref: a.target_ref,
        state_file: a.state,
    };
    let report = import(&opts, |i, n| {
        if i % 100 == 0 || i == n {
            eprintln!("[{i}/{n}] commits");
        }
    })
    .await?;
    match report.tip {
        Some(t) => println!(
            "Imported {} commits ({} already present); {} -> {}",
            report.imported,
            report.skipped,
            opts.target_ref,
            t.short()
        ),
        None => println!("No commits to import"),
    }
    Ok(())
}
