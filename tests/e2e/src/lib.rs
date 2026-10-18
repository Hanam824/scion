//! Test harness: an in-process server on an ephemeral port.

use scion_repo::Repo;
use std::sync::Arc;

pub struct TestServer {
    pub url: String,
    pub repo: Arc<Repo>,
    _data: tempfile::TempDir,
}

pub async fn start_server() -> TestServer {
    let data = tempfile::tempdir().unwrap();
    let repo = Arc::new(Repo::open_local(data.path()).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(scion_server::serve(repo.clone(), listener));
    TestServer {
        url,
        repo,
        _data: data,
    }
}
