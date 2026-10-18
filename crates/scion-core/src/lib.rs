//! Core data model for Scion: content-addressed ids, trees, changesets, paths,
//! and text merging. Has no I/O; storage lives in `scion-store`.

pub mod id;
pub mod merge;
pub mod model;
pub mod path;
pub mod text;

pub use id::{InvalidId, ObjectId};
pub use merge::{merge_text, MergeOutcome};
pub use model::{
    Change, Changeset, FileMode, FileOp, HistEntry, HistTree, Object, Tree, TreeEntry,
};
pub use path::{join, validate_path, PathFilter};
