//! nfscope: a live `top`-style view of NFS server client I/O.

pub mod app;
pub mod files;
pub mod fmt;
pub mod inflight;
pub mod model;
pub mod netlink;
pub mod parse;
#[cfg(target_os = "linux")]
pub mod sockdiag;
pub mod source;
pub mod ui;
pub mod workers;
