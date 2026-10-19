# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust reimplementation (Core MVP) of Scion, the server-centric source control system described in Anthropic's "Redesigning source control" post. Clients are thin and always online. Commits, merges and rebases execute **on the server**, so there is no push. History is a log of changesets that list per-file changes relative to their first parent. Everything else (trees, blame, path history, graph skip lists) is derived data.

## Commands

```sh
cargo build --workspace
cargo test --workspace                                # all tests (~10s; scion-repo deep-history test dominates)
cargo test -p scion-repo merge::tests::land_fast_forward_rebase_and_conflict   # single test
cargo test -p e2e                                     # end-to-end: in-process gRPC server + real working copies
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Run it by hand:

```sh
cargo run -p scion-server -- --data ./scion-data --listen 127.0.0.1:7777
cargo run -p scion-import -- <git-repo> --server http://127.0.0.1:7777 --ref main
cargo run -p scn -- clone http://127.0.0.1:7777 work --sparse src
cd work && scn status / commit -m ... [PATHS] / land [--resolve] / sync / log [path] / blame <path> / diff [A [B]] / show
         scn branch list|create|switch|rename|delete|merge / revert REV / restore PATHS / resolve PATH [--mine|--theirs] / abort / sparse set ...
cargo run -p scion-desktop                            # GUI (Slint); --release binary is ~25 MB
```

`protoc` is vendored (`protoc-bin-vendored` in `crates/scion-proto/build.rs`), so no system install is needed. Editing `proto/scion.proto` regenerates the bindings on the next build.

## CI and releases

- `.github/workflows/ci.yml` runs on pull requests to `main`. fmt and clippy run once on Linux; build and test run on macOS arm64/Intel, Linux and Windows. `CI result` is the single check to require on `main`. Docs-only PRs skip the build; `scripts/code-changes.sh` defines what counts as docs.
- `.github/workflows/nightly-release.yml` runs at 17:00 UTC when a PR was merged in the last 24 h (or on a forced manual run). `scripts/next-version.sh` bumps the patch of the last `vX.Y.Z` tag. `scripts/package-release.sh <tag> [platform]` builds and archives the four binaries into `dist/`.
- `.github/actions/rust-setup` installs the toolchain, the Linux libraries Slint needs, and the cargo cache.
- Commits and branches follow Conventional Commits; see `.claude/skills/create-commit-message` and `create-commit-branch`. Never add `Co-Authored-By` or AI attribution.

## Architecture

The dependency chain is `scion-core` → `scion-store` → `scion-repo` → `scion-server`. `scion-proto` is shared by the server, `scn` and `scion-import`.

- **scion-core**: pure data model with no I/O.
  - `ObjectId` is a BLAKE3 hash (hex in serde).
  - `Changeset` holds parents, metadata and `changes: Vec<Change>`, relative to `parents[0]` only.
  - `Tree` is one directory level. `HistTree` mirrors a tree, recording the last changeset that touched each path.
  - Objects implement `Object`, a canonical JSON encoding with fixed field order and BTreeMaps, so the id is the hash of the encoding. Changing any serialized struct changes every id.
  - `merge.rs` is the diff3 line merge. Edits that touch or overlap conflict, as in git.
- **scion-store**: the `MetaStore` and `BlobStore` traits, with `SqliteMeta` and `FsBlobs` (zstd) implementations. This is where Spanner and object storage would plug in. Refs move **only** through `cas_ref` (compare-and-swap).
- **scion-repo**: the server engine. Synchronous code; the server calls it via `spawn_blocking`.
  - `derive.rs` is the generic derived-data framework. `Derivation::derive(cs, parent_values)` is memoized in the `derived` table, and `Repo::derive::<D>` computes missing ancestors iteratively, never recursively.
  - Eager kinds, computed in `create_changeset`: `RootTree`, `HistRoot` and `Graph` (generation number plus first-parent skip list).
  - Blame (`blame.rs`) is a lazy derivation keyed by `(changeset, path)`.
  - `tree.rs` holds `apply_changes` and `diff_trees`. Both only visit changed directories, so cost scales with the change, not the repo. `apply_changes` is also where changesets are validated (file/dir clashes, deleting missing files). Empty directories are pruned.
  - `graph.rs` holds `merge_base` (generation-ordered walk), `is_ancestor`, `first_parent_ancestor` (skip lists) and `log`. `log <path>` follows `HistTree.last` along first parents.
  - `merge.rs` holds `three_way` (tree-level), `rebase`, and `land`. `land` is lock-free: it computes against the current ref, then CASes, retrying up to 32 times. There is deliberately no merge queue. The default strategy is rebase (linear history); a fast-forward keeps the source id.
- **scion-server**: tonic service. Handlers parse, run, and convert. `RepoError::{NotFound, Invalid}` map to gRPC codes. Revision strings accept refs, hex ids or prefixes (at least 4 characters), and `~N`.
- **scn**: the client, a library (`ops.rs::Workspace`, one method per command) plus a thin clap CLI in `main.rs`.
  - Working-copy state lives in `.scn/state.json`: base changeset, tracked ref, sparse prefixes, and per-file blob/size/mtime used for change detection without rehashing.
  - `.scn/cache` is a bounded LRU blob cache (`SCN_CACHE_MAX_BYTES`).
  - `commit` uploads only missing blobs (HasBlobs, then PutBlobs) and creates a changeset without moving refs. `land` merges on the server, then syncs.
  - `sync` applies a server-computed `Diff` restricted to the sparse prefixes, and refuses to overwrite dirty paths.
  - Checkouts walk trees from the root one level per round trip (`GetTrees`), with no packfiles.
  - `.scnignore` lists path component names to skip.
  - Branches are refs. Committing never moves a ref; `land` does. `switch_branch` refuses while the base has unlanded changesets (or the branch is unpublished), because switching would orphan them.
  - Conflicts: `land`/`rebase`/`revert`/`merge_branch` return a `ConflictSet` carrying the server's cleanly merged changes (`MergeResponse.resolved`). `start_resolution` syncs to the target, writes those changes plus conflict markers (or the source side for delete/binary conflicts) as local modifications, and records `state.conflicts`. `commit` refuses while `unresolved()` is non-empty.
- **scion-import**: drives `git log`, `diff-tree` and `cat-file --batch`, and talks to the server only through the public RPCs. Resumable via a sha→changeset mapping file. Changeset ids are deterministic, so re-runs are idempotent.
- **scion-desktop**: Slint GUI modelled on GitHub Desktop.
  - Everything except `main.rs` is UI-free and tested headless (`tests/worker.rs`).
  - `worker.rs` owns the `Workspace` on a background tokio thread: `Command` in, plain-data `Event` out through a `Sink`. It also runs a 60 s `Poll` for ahead/behind.
  - `main.rs` maps events onto Slint properties. UI-only state (unchecked files, last diff, pending confirmation) lives in a `thread_local`, and unified/split/wrap re-render from the last diff without a worker round trip.
  - `ui/app.slint` is the window, including the `MenuBar` (native on macOS, so `@keys(Control + …)` means Cmd there). Custom popup rows are `PopupItem`, because `MenuItem` is a Slint builtin.
  - `ui/widgets.slint` holds the `Colors` and `Syntax` globals and the virtualized `DiffView`/`SplitDiffView`.
  - `highlight.rs` maps syntect scopes onto a few classes, and `SynClass` must match `Syntax.color` in the .slint file. The `DiffLine.kind` codes must match `model::LineKind`.
  - `watch.rs` (notify, 300 ms debounce) triggers refreshes. The built-in server listens on 127.0.0.1:7787, with data in the platform data dir.
- **tests/e2e**: `e2e::start_server()` gives an ephemeral-port server plus a `Repo` handle for asserting on refs directly.

## Not built yet (from the article; the RPC surface is designed to allow them)

- The `scion-daemon` and `scion-cache` tiers, which would serve the same RPCs.
- FUSE or virtualized checkouts, and a filesystem watcher.
- Per-directory ACLs.
- A Postgres/S3 `MetaStore`/`BlobStore`.
- Chunking for blobs larger than the 256 MB gRPC message limit (`MAX_MESSAGE`).
