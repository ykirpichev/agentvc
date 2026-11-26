//! Core of agentvc: the store, the task/attempt/snapshot model and
//! copy-on-write views. See `docs/design.md`.

pub mod bridge;
pub mod bundle;
pub mod combine;
pub mod config;
pub mod cursor;
pub mod doctor;
pub mod error;
pub mod export;
pub mod gc;
pub mod github;
pub mod hooks;
pub mod index;
pub mod objects;
#[cfg(target_os = "linux")]
pub mod overlay;
pub mod overview;
pub mod patch;
pub mod rebase;
pub mod run;
pub mod snapshot;
pub mod space;
pub mod status;
pub mod story;
pub mod sync;
pub mod tags;
pub mod view;
pub mod viewgit;

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
pub use viewgit::ViewGit;

/// Version of the on-disk format written by this build.
pub const FORMAT_VERSION: u32 = 1;
