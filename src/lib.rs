pub mod bundle;
pub mod catalog;
pub mod chunker;
pub mod claims;
pub mod coord;
pub mod fork;
pub mod gc;
pub mod mcp;
pub mod merge;
pub mod model;
pub mod path_index;
pub mod refs;
pub mod remote;
pub mod remote_crypto;
pub mod repository;
pub mod shrink;
pub mod sorted_dir;
pub mod sqlite_adapter;
pub mod store;
pub mod sync;
pub mod tree;
pub mod watcher;

pub use gc::GcReport;
pub use repository::{
    AgitRepository, ClaimOutcome, CoordOutcome, DiffChange, DiffSummary, ForkPlan, ForkRemoval,
    ForkSummary, ForkUpdates, MergeOutcome, ReleaseOutcome, RepositoryStatus, RewindPlan,
    SnapshotSummary, SyncDisposition, SyncPullOutcome,
};
