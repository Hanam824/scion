//! Generated gRPC types plus conversions and client helpers shared by `scn`
//! and `scion-import`.

#[allow(clippy::all)]
pub mod pb {
    tonic::include_proto!("scion.v1");
}

use anyhow::{Context, Result};
use scion_core::{Change, FileMode, FileOp, ObjectId};
use tonic::transport::Channel;

pub use pb::scion_client::ScionClient;
pub use pb::scion_server::{Scion, ScionServer};

pub type Client = ScionClient<Channel>;

/// Max gRPC message size; a single blob must fit in one message.
pub const MAX_MESSAGE: usize = 256 << 20;

pub async fn connect(url: &str) -> Result<Client> {
    let client = ScionClient::connect(url.to_string())
        .await
        .with_context(|| format!("connecting to {url}"))?;
    Ok(client
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE))
}

pub fn mode_to_pb(m: FileMode) -> i32 {
    (match m {
        FileMode::Regular => pb::FileMode::Regular,
        FileMode::Executable => pb::FileMode::Executable,
        FileMode::Symlink => pb::FileMode::Symlink,
    }) as i32
}

pub fn mode_from_pb(m: i32) -> FileMode {
    match pb::FileMode::try_from(m) {
        Ok(pb::FileMode::Executable) => FileMode::Executable,
        Ok(pb::FileMode::Symlink) => FileMode::Symlink,
        _ => FileMode::Regular,
    }
}

pub fn change_to_pb(c: &Change) -> pb::FileChange {
    match c.op {
        FileOp::Upsert { blob, mode } => pb::FileChange {
            path: c.path.clone(),
            delete: false,
            blob: blob.to_hex(),
            mode: mode_to_pb(mode),
        },
        FileOp::Delete => pb::FileChange {
            path: c.path.clone(),
            delete: true,
            ..Default::default()
        },
    }
}

pub fn change_from_pb(c: &pb::FileChange) -> Result<Change, scion_core::InvalidId> {
    let op = if c.delete {
        FileOp::Delete
    } else {
        FileOp::Upsert {
            blob: ObjectId::from_hex(&c.blob)?,
            mode: mode_from_pb(c.mode),
        }
    };
    Ok(Change {
        path: c.path.clone(),
        op,
    })
}

pub fn parse_id(s: &str) -> Result<ObjectId> {
    Ok(ObjectId::from_hex(s)?)
}

/// Empty string means "none".
pub fn parse_opt_id(s: &str) -> Result<Option<ObjectId>> {
    if s.is_empty() {
        Ok(None)
    } else {
        parse_id(s).map(Some)
    }
}

/// Which of `ids` the server already has.
pub async fn has_blobs(client: &mut Client, ids: &[ObjectId]) -> Result<Vec<bool>> {
    let mut present = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(4096) {
        let req = pb::HasBlobsRequest {
            ids: chunk.iter().map(|i| i.to_hex()).collect(),
        };
        present.extend(client.has_blobs(req).await?.into_inner().present);
    }
    Ok(present)
}

pub async fn put_blobs(client: &mut Client, blobs: Vec<(ObjectId, Vec<u8>)>) -> Result<()> {
    if blobs.is_empty() {
        return Ok(());
    }
    let msgs: Vec<pb::BlobData> = blobs
        .into_iter()
        .map(|(id, data)| pb::BlobData {
            id: id.to_hex(),
            data,
        })
        .collect();
    client.put_blobs(tokio_stream::iter(msgs)).await?;
    Ok(())
}
