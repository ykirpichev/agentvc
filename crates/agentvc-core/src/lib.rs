//! Core of agentvc: the store, the task/attempt/snapshot model and
//! copy-on-write views. See `docs/design.md`.

/// Version of the on-disk format written by this build.
pub const FORMAT_VERSION: u32 = 1;
