//! `scn`, the thin Scion client: a sparse working copy, a bounded local
//! object cache, and change detection. Commits, merges and rebases happen
//! on the server, so there is nothing to push.

pub mod cache;
pub mod ops;
pub mod state;
pub mod worktree;

pub use ops::Workspace;
