//! gRPC front end for `scion_repo::Repo`. Handlers are thin: parse, run the
//! synchronous repo operation on a blocking thread, convert the result.

use anyhow::Result;
use scion_core::{text::unified_patch, Changeset, ObjectId, PathFilter, Tree, TreeEntry};
use scion_proto::{
    change_from_pb, change_to_pb, mode_to_pb, parse_opt_id, pb, Scion, ScionServer, MAX_MESSAGE,
};
use scion_repo::{
    blame, empty_tree_id, graph, invalid, merge, Graph, LandResult, Repo, RepoError, RootTree,
    Strategy,
};
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

pub async fn serve(repo: Arc<Repo>, listener: TcpListener) -> Result<()> {
    let svc = ScionServer::new(Service { repo })
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    tonic::transport::Server::builder()
        .add_service(svc)
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}

pub struct Service {
    repo: Arc<Repo>,
}

fn status(e: anyhow::Error) -> Status {
    match e.downcast_ref::<RepoError>() {
        Some(RepoError::NotFound(m)) => Status::not_found(m.clone()),
        Some(RepoError::Invalid(m)) => Status::invalid_argument(m.clone()),
        None => Status::internal(format!("{e:#}")),
    }
}

fn pid(s: &str) -> Result<ObjectId> {
    ObjectId::from_hex(s).map_err(|e| invalid(e.to_string()))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn info(
    repo: &Repo,
    id: ObjectId,
    cs: &Changeset,
    with_changes: bool,
) -> Result<pb::ChangesetInfo> {
    Ok(pb::ChangesetInfo {
        id: id.to_hex(),
        parents: cs.parents.iter().map(|p| p.to_hex()).collect(),
        author: cs.author.clone(),
        message: cs.message.clone(),
        timestamp: cs.timestamp,
        changes: if with_changes {
            cs.changes.iter().map(change_to_pb).collect()
        } else {
            vec![]
        },
        root_tree: repo.derive::<RootTree>(&id)?.to_hex(),
        generation: repo.derive::<Graph>(&id)?.generation,
    })
}

fn tree_to_pb(id: &ObjectId, t: &Tree) -> pb::Tree {
    let entries = t
        .entries
        .iter()
        .map(|(name, e)| match e {
            TreeEntry::File { blob, mode } => pb::TreeEntry {
                name: name.clone(),
                is_dir: false,
                id: blob.to_hex(),
                mode: mode_to_pb(*mode),
            },
            TreeEntry::Dir { tree } => pb::TreeEntry {
                name: name.clone(),
                is_dir: true,
                id: tree.to_hex(),
                mode: 0,
            },
        })
        .collect();
    pb::Tree {
        id: id.to_hex(),
        entries,
    }
}

fn merge_response(r: LandResult) -> pb::MergeResponse {
    match r {
        LandResult::UpToDate(id) => pb::MergeResponse {
            id: id.to_hex(),
            up_to_date: true,
            attempts: 1,
            ..Default::default()
        },
        LandResult::Landed {
            id,
            fast_forward,
            attempts,
        } => pb::MergeResponse {
            id: id.to_hex(),
            fast_forward,
            attempts,
            ..Default::default()
        },
        LandResult::Conflicts(c) => conflicted_response(c),
    }
}

fn version_to_pb(v: merge::FileVersion) -> (String, i32) {
    v.map(|(b, m)| (b.to_hex(), mode_to_pb(m)))
        .unwrap_or_default()
}

/// A conflicted merge: the conflicts plus everything that merged cleanly.
fn conflicted_response(c: merge::Conflicted) -> pb::MergeResponse {
    let conflicts = c
        .conflicts
        .into_iter()
        .map(|c| {
            let (target_blob, target_mode) = version_to_pb(c.target);
            let (source_blob, source_mode) = version_to_pb(c.source);
            pb::Conflict {
                path: c.path,
                kind: c.kind,
                detail: c.detail,
                target_blob,
                target_mode,
                source_blob,
                source_mode,
            }
        })
        .collect();
    pb::MergeResponse {
        conflicts,
        target: c.target.to_hex(),
        resolved: c.resolved.iter().map(change_to_pb).collect(),
        ..Default::default()
    }
}

/// Response for operations that produce a single new changeset.
fn outcome_response(o: merge::Outcome, onto: ObjectId) -> pb::MergeResponse {
    match o {
        Ok(id) => pb::MergeResponse {
            id: id.to_hex(),
            up_to_date: id == onto,
            ..Default::default()
        },
        Err(c) => conflicted_response(c),
    }
}

impl Service {
    // `Status` is large, but it is the error type tonic's handlers return.
    #[allow(clippy::result_large_err)]
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Repo) -> Result<T> + Send + 'static,
    ) -> Result<T, Status> {
        let repo = self.repo.clone();
        tokio::task::spawn_blocking(move || f(&repo))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(status)
    }
}

#[tonic::async_trait]
impl Scion for Service {
    async fn has_blobs(
        &self,
        req: Request<pb::HasBlobsRequest>,
    ) -> Result<Response<pb::HasBlobsResponse>, Status> {
        let ids = req.into_inner().ids;
        let present = self
            .run(move |r| ids.iter().map(|s| r.has_blob(&pid(s)?)).collect())
            .await?;
        Ok(Response::new(pb::HasBlobsResponse { present }))
    }

    async fn put_blobs(
        &self,
        req: Request<Streaming<pb::BlobData>>,
    ) -> Result<Response<pb::PutBlobsResponse>, Status> {
        let mut stream = req.into_inner();
        let mut stored = 0;
        while let Some(msg) = stream.message().await? {
            self.run(move |r| r.put_blob_checked(&pid(&msg.id)?, &msg.data))
                .await?;
            stored += 1;
        }
        Ok(Response::new(pb::PutBlobsResponse { stored }))
    }

    type GetBlobsStream = ReceiverStream<Result<pb::BlobData, Status>>;

    async fn get_blobs(
        &self,
        req: Request<pb::GetBlobsRequest>,
    ) -> Result<Response<Self::GetBlobsStream>, Status> {
        let ids = req.into_inner().ids;
        let (tx, rx) = mpsc::channel(32);
        let repo = self.repo.clone();
        tokio::spawn(async move {
            for s in ids {
                let repo = repo.clone();
                let res = tokio::task::spawn_blocking(move || -> Result<pb::BlobData> {
                    let data = repo.get_blob(&pid(&s)?)?;
                    Ok(pb::BlobData { id: s, data })
                })
                .await;
                let item = match res {
                    Ok(Ok(b)) => Ok(b),
                    Ok(Err(e)) => Err(status(e)),
                    Err(e) => Err(Status::internal(e.to_string())),
                };
                let failed = item.is_err();
                if tx.send(item).await.is_err() || failed {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_trees(
        &self,
        req: Request<pb::GetTreesRequest>,
    ) -> Result<Response<pb::GetTreesResponse>, Status> {
        let ids = req.into_inner().ids;
        let trees = self
            .run(move |r| {
                ids.iter()
                    .map(|s| {
                        let id = pid(s)?;
                        Ok(tree_to_pb(&id, &r.get_tree::<Tree>(&id)?))
                    })
                    .collect()
            })
            .await?;
        Ok(Response::new(pb::GetTreesResponse { trees }))
    }

    async fn create_changeset(
        &self,
        req: Request<pb::CreateChangesetRequest>,
    ) -> Result<Response<pb::CreateChangesetResponse>, Status> {
        let req = req.into_inner();
        let (id, root) = self
            .run(move |r| {
                let cs = Changeset {
                    parents: req.parents.iter().map(|p| pid(p)).collect::<Result<_>>()?,
                    author: req.author,
                    message: req.message,
                    timestamp: if req.timestamp == 0 {
                        now()
                    } else {
                        req.timestamp
                    },
                    changes: req
                        .changes
                        .iter()
                        .map(|c| change_from_pb(c).map_err(|e| invalid(e.to_string())))
                        .collect::<Result<_>>()?,
                };
                let id = r.create_changeset(cs)?;
                Ok((id, r.derive::<RootTree>(&id)?))
            })
            .await?;
        Ok(Response::new(pb::CreateChangesetResponse {
            id: id.to_hex(),
            root_tree: root.to_hex(),
        }))
    }

    async fn get_changeset(
        &self,
        req: Request<pb::GetChangesetRequest>,
    ) -> Result<Response<pb::ChangesetInfo>, Status> {
        let rev = req.into_inner().rev;
        let out = self
            .run(move |r| {
                let id = r.resolve(&rev)?;
                info(r, id, &r.get_changeset(&id)?, true)
            })
            .await?;
        Ok(Response::new(out))
    }

    async fn resolve(
        &self,
        req: Request<pb::ResolveRequest>,
    ) -> Result<Response<pb::ResolveResponse>, Status> {
        let rev = req.into_inner().rev;
        let (id, root) = self
            .run(move |r| {
                let id = r.resolve(&rev)?;
                Ok((id, r.derive::<RootTree>(&id)?))
            })
            .await?;
        Ok(Response::new(pb::ResolveResponse {
            id: id.to_hex(),
            root_tree: root.to_hex(),
        }))
    }

    async fn list_refs(
        &self,
        _req: Request<pb::ListRefsRequest>,
    ) -> Result<Response<pb::ListRefsResponse>, Status> {
        let refs = self.run(|r| r.meta().list_refs()).await?;
        let refs = refs
            .into_iter()
            .map(|(name, t)| pb::Ref {
                name,
                target: t.to_hex(),
            })
            .collect();
        Ok(Response::new(pb::ListRefsResponse { refs }))
    }

    async fn update_ref(
        &self,
        req: Request<pb::UpdateRefRequest>,
    ) -> Result<Response<pb::UpdateRefResponse>, Status> {
        let req = req.into_inner();
        let (ok, current) = self
            .run(move |r| {
                let expected = parse_opt_id(&req.expected).map_err(|e| invalid(e.to_string()))?;
                let new = if req.new_target.is_empty() {
                    None
                } else {
                    Some(r.resolve(&req.new_target)?)
                };
                let ok = r.meta().cas_ref(&req.name, expected, new)?;
                Ok((ok, r.meta().get_ref(&req.name)?))
            })
            .await?;
        Ok(Response::new(pb::UpdateRefResponse {
            ok,
            current: current.map(|c| c.to_hex()).unwrap_or_default(),
        }))
    }

    async fn merge(
        &self,
        req: Request<pb::MergeRequest>,
    ) -> Result<Response<pb::MergeResponse>, Status> {
        let req = req.into_inner();
        let out = self
            .run(move |r| {
                let source = r.resolve(&req.source)?;
                let strategy = match pb::MergeStrategy::try_from(req.strategy) {
                    Ok(pb::MergeStrategy::Merge) => Strategy::Merge,
                    _ => Strategy::Rebase,
                };
                let message = if req.message.is_empty() {
                    format!("Merge {} into {}", source.short(), req.target_ref)
                } else {
                    req.message
                };
                merge::land(
                    r,
                    &req.target_ref,
                    source,
                    strategy,
                    &req.author,
                    &message,
                    now(),
                )
            })
            .await?;
        Ok(Response::new(merge_response(out)))
    }

    async fn rebase(
        &self,
        req: Request<pb::RebaseRequest>,
    ) -> Result<Response<pb::MergeResponse>, Status> {
        let req = req.into_inner();
        let out = self
            .run(move |r| {
                let (source, onto) = (r.resolve(&req.source)?, r.resolve(&req.onto)?);
                Ok(outcome_response(merge::rebase(r, source, onto)?, onto))
            })
            .await?;
        Ok(Response::new(out))
    }

    async fn revert(
        &self,
        req: Request<pb::RevertRequest>,
    ) -> Result<Response<pb::MergeResponse>, Status> {
        let req = req.into_inner();
        let out = self
            .run(move |r| {
                let (cs, onto) = (r.resolve(&req.changeset)?, r.resolve(&req.onto)?);
                let o = merge::revert(r, cs, onto, &req.author, now())?;
                Ok(outcome_response(o, onto))
            })
            .await?;
        Ok(Response::new(out))
    }

    async fn compare(
        &self,
        req: Request<pb::CompareRequest>,
    ) -> Result<Response<pb::CompareResponse>, Status> {
        let req = req.into_inner();
        let r = self
            .run(move |r| graph::ahead_behind(r, r.resolve(&req.a)?, r.resolve(&req.b)?, 10_000))
            .await?;
        Ok(Response::new(pb::CompareResponse {
            ahead: r.ahead,
            behind: r.behind,
            truncated: r.truncated,
        }))
    }

    async fn diff(
        &self,
        req: Request<pb::DiffRequest>,
    ) -> Result<Response<pb::DiffResponse>, Status> {
        let req = req.into_inner();
        let files = self
            .run(move |r| {
                let from = if req.from.is_empty() {
                    empty_tree_id()
                } else {
                    r.derive::<RootTree>(&r.resolve(&req.from)?)?
                };
                let to = r.derive::<RootTree>(&r.resolve(&req.to)?)?;
                let diffs =
                    scion_repo::tree::diff_trees(r, from, to, &PathFilter::new(&req.paths))?;
                diffs
                    .into_iter()
                    .map(|d| {
                        let patch = if req.patch {
                            let load = |x: Option<(ObjectId, _)>| {
                                x.map(|(b, _)| r.get_blob(&b)).transpose()
                            };
                            let (old, new) = (load(d.old)?, load(d.new)?);
                            unified_patch(&d.path, old.as_deref(), new.as_deref())
                        } else {
                            String::new()
                        };
                        Ok(pb::FileDiff {
                            path: d.path,
                            old_blob: d.old.map(|x| x.0.to_hex()).unwrap_or_default(),
                            old_mode: d.old.map(|x| mode_to_pb(x.1)).unwrap_or_default(),
                            new_blob: d.new.map(|x| x.0.to_hex()).unwrap_or_default(),
                            new_mode: d.new.map(|x| mode_to_pb(x.1)).unwrap_or_default(),
                            patch,
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
        Ok(Response::new(pb::DiffResponse { files }))
    }

    async fn merge_base(
        &self,
        req: Request<pb::MergeBaseRequest>,
    ) -> Result<Response<pb::MergeBaseResponse>, Status> {
        let req = req.into_inner();
        let base = self
            .run(move |r| graph::merge_base(r, r.resolve(&req.a)?, r.resolve(&req.b)?))
            .await?;
        Ok(Response::new(pb::MergeBaseResponse {
            id: base.map(|b| b.to_hex()).unwrap_or_default(),
        }))
    }

    async fn log(&self, req: Request<pb::LogRequest>) -> Result<Response<pb::LogResponse>, Status> {
        let req = req.into_inner();
        let changesets = self
            .run(move |r| {
                let start = r.resolve(&req.rev)?;
                let limit = if req.limit == 0 {
                    50
                } else {
                    req.limit as usize
                };
                let path = Some(req.path.trim_matches('/')).filter(|p| !p.is_empty());
                graph::log(r, start, path, limit)?
                    .into_iter()
                    .map(|(id, cs)| info(r, id, &cs, false))
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
        Ok(Response::new(pb::LogResponse { changesets }))
    }

    async fn blame(
        &self,
        req: Request<pb::BlameRequest>,
    ) -> Result<Response<pb::BlameResponse>, Status> {
        let req = req.into_inner();
        let lines = self
            .run(move |r| {
                let rev = r.resolve(&req.rev)?;
                let mut cache = std::collections::HashMap::new();
                blame::blame(r, rev, &req.path)?
                    .into_iter()
                    .map(|l| {
                        let cs = match cache.entry(l.changeset) {
                            Entry::Occupied(e) => e.into_mut(),
                            Entry::Vacant(v) => v.insert(r.get_changeset(&l.changeset)?),
                        };
                        Ok(pb::BlameLine {
                            changeset: l.changeset.to_hex(),
                            author: cs.author.clone(),
                            timestamp: cs.timestamp,
                            text: l.text,
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
        Ok(Response::new(pb::BlameResponse { lines }))
    }
}
