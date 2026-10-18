//! Built-in server for a zero-setup first run. It uses its own port so it is
//! never confused with a separately started `scion-server` on 7777.

use anyhow::Result;
use scion_repo::Repo;
use std::path::Path;
use std::sync::Arc;

pub const ADDR: &str = "127.0.0.1:7787";
pub const URL: &str = "http://127.0.0.1:7787";

/// Starts the built-in server on the current tokio runtime, unless something
/// is already listening on its port (e.g. another app instance).
pub async fn ensure_running(data: &Path) -> Result<()> {
    match tokio::net::TcpListener::bind(ADDR).await {
        Ok(listener) => {
            let repo = Arc::new(Repo::open_local(data)?);
            tokio::spawn(async move {
                if let Err(e) = scion_server::serve(repo, listener).await {
                    eprintln!("built-in server stopped: {e:#}");
                }
            });
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Ok(()),
        Err(e) => Err(e.into()),
    }
}
