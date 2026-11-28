//! Core of agentvc: the store, the task/attempt/snapshot model and
//! copy-on-write views. See `docs/design.md`.

pub mod agent;
pub mod agent_setup;
pub mod annotate;
pub mod blame;
pub mod bridge;
pub mod browse;
pub mod bundle;
pub mod check;
pub mod checkouts;
pub mod combine;
pub mod comments;
pub mod config;
pub mod conflicts;
pub mod cursor;
pub mod doctor;
pub mod error;
pub mod export;
pub mod feedback;
pub mod gc;
pub mod github;
pub mod hooks;
pub mod import;
pub mod index;
pub mod land;
pub mod objects;
pub mod overlaps;
#[cfg(target_os = "linux")]
pub mod overlay;
pub mod overview;
pub mod patch;
pub mod query;
pub mod rebase;
pub mod relay;
pub mod resolve;
pub mod restack;
pub mod run;
pub mod snapshot;
pub mod space;
pub mod stack;
pub mod stackundo;
pub mod status;
pub mod story;
pub mod sync;
pub mod syncmerge;
pub mod tags;
pub mod tool_model;
pub mod undo;
pub mod view;
pub mod viewgit;
pub mod workspace;

#[cfg(test)]
pub(crate) mod testutil;

pub use error::{Error, Result};
pub use export::{Export, ExportOptions, ExportResult};
pub use github::{PrOptions, PrResult};
pub use overview::Overview;
pub use rebase::{GitBase, RebaseOptions, RebaseResult};
pub use space::{
    Attempt, ForkRequest, ForkResult, Ref, Snapshot, SnapshotResult, Space, Status, Task,
};
pub use sync::{CloneOptions, CloneResult, RemoteSpace, SyncOptions, SyncResult};
pub use undo::{Operation, UndoResult};
pub use viewgit::ViewGit;

/// Version of the on-disk format written by this build.
pub const FORMAT_VERSION: u32 = 1;
