# Scion

A Rust reimplementation of **Scion**, the server-centric source control system described in Anthropic's [Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control). It comes with a GitHub-Desktop-style desktop app.

Unlike git, clients are thin and always online. Commits, merges and rebases run **on the server**, so there is no `push`. History is a log of *changesets*, each listing per-file changes relative to its first parent. Trees, blame, path history and graph skip lists are all derived from that log.

> Status: Core MVP. The single-server engine, the CLI client, a git importer and the desktop app work end to end. The caching tiers, virtual filesystem and ACLs from the article are not built yet (see [Roadmap](#roadmap)).

## Features

- **Content-addressed changesets.** Objects are hashed with BLAKE3 over a canonical JSON encoding, so ids are deterministic.
- **Lock-free landing.** Refs move only by compare-and-swap. `land` merges against the current tip and retries on contention, with no merge queue. The default strategy is rebase (linear history).
- **Server-side operations:** three-way merge (diff3), rebase, revert, merge-base and ahead/behind.
- **Derived data,** computed on demand and memoized:
  - root trees;
  - per-path history;
  - a commit graph with skip lists;
  - lazy blame.
- **`scn` client:**
  - sparse checkouts;
  - a bounded local blob cache;
  - partial commits;
  - branches;
  - conflict resolution in the working copy.
- **Git importer** that is resumable and idempotent.
- **Scion Desktop**, a cross-platform GUI built with [Slint](https://slint.dev):
  - the changes list with partial commits and a unified or split diff with syntax highlighting;
  - history and blame;
  - branches, plus conflict resolution;
  - a native menu bar and a resizable sidebar;
  - auto-refresh when files change.

## Quick start

Requires a recent stable Rust toolchain. `protoc` is vendored, so nothing else needs installing.

```sh
cargo build --release --workspace
export PATH="$PWD/target/release:$PATH"

# 1. Start a server
scion-server --data ./scion-data --listen 127.0.0.1:7777

# 2. (optional) Import an existing git repository
scion-import /path/to/git-repo --server http://127.0.0.1:7777 --ref main

# 3. Clone a working copy (optionally sparse)
scn clone http://127.0.0.1:7777 work            # or: --sparse src
cd work
```

Daily workflow:

```sh
scn status                       # changed files, ahead/behind, conflicts
scn commit -m "message" [PATHS]  # creates a changeset; does not move any ref
scn land                         # merge on the server, then sync
scn sync                         # bring the working copy up to date
scn log [path]  |  scn blame <path>  |  scn diff [A [B]]  |  scn show [REV]
```

Branches and conflicts:

```sh
scn branch list | create NAME | switch NAME | rename OLD NEW | delete NAME | merge NAME
scn revert REV
scn restore PATHS                # discard local changes
scn land --resolve               # on conflict, write markers into the working copy
scn resolve PATH [--mine|--theirs]
scn abort
scn sparse set src docs
```

Revisions accept ref names, full or prefix hex ids (at least 4 characters) and `~N` suffixes.

## Desktop app

```sh
cargo run --release -p scion-desktop
```

Open an existing `scn` working copy, or clone one. The clone dialog can start a **built-in local server** (127.0.0.1:7787, with data in the platform data dir), so you can try it with zero setup. The app works on macOS, Windows and Linux. On macOS the menu bar is native, and shortcuts use ⌘.

## Architecture

```
scion-core ─► scion-store ─► scion-repo ─► scion-server ◄─ gRPC ─► scn (CLI + library) ─► scion-desktop
                                                         ◄─ gRPC ─► scion-import
```

| Crate | Role |
|---|---|
| `scion-core` | Pure data model with no I/O: ids, trees, changesets, diff3 line merge, patches |
| `scion-store` | `MetaStore` and `BlobStore` traits, implemented on SQLite and the filesystem (zstd). This is where cloud backends would plug in |
| `scion-repo` | The engine: derived-data framework, tree diff/apply, graph queries, merge/rebase/land, blame |
| `scion-proto` | Protobuf/gRPC definitions (`proto/scion.proto`) and conversions |
| `scion-server` | tonic gRPC service over `scion-repo` |
| `scn` | Client library (`Workspace`) and thin CLI |
| `scion-import` | git → Scion importer that uses only the public RPCs |
| `scion-desktop` | Slint GUI; a background worker drives `scn::Workspace` |
| `tests/e2e` | End-to-end tests against an in-process server and real working copies |

See [CLAUDE.md](CLAUDE.md) for a more detailed architecture walkthrough.

## Development

```sh
cargo test --workspace                                # all tests
cargo test -p e2e                                     # end-to-end only
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Roadmap

Not built yet, though the RPC surface is designed to allow them:

- **Caching tiers:** `scion-daemon` and `scion-cache`, serving the same RPCs.
- **Virtualized checkouts:** FUSE or similar.
- **Per-directory ACLs.**
- **Cloud backends:** a Postgres/S3 metadata and blob store.
- **Large blobs:** chunking for blobs larger than 256 MB.
- **Desktop packaging:** `.app`/`.dmg`, `.msi`, `.deb`/AppImage.
