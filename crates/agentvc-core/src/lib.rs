//! Core of agentvc: the store, the task/attempt/snapshot model and
//! copy-on-write views. See `docs/design.md`.

pub mod error;
pub mod export;
pub mod github;
pub mod hooks;
pub mod index;
pub mod objects;
#[cfg(target_os = "linux")]
pub mod overlay;
pub mod run;
pub mod snapshot;
pub mod space;
pub mod view;

#[cfg(test)]
pub(crate) mod testutil;

pub use error::{Error, Result};
pub use export::{Export, ExportOptions, ExportResult};
pub use github::{PrOptions, PrResult};
pub use space::{
    Attempt, ForkRequest, ForkResult, Ref, Snapshot, SnapshotResult, Space, Status, Task,
};

/// Version of the on-disk format written by this build.
pub const FORMAT_VERSION: u32 = 1;
