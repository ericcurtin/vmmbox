//! A virtio-fs file server for macOS, run as a separate process that QEMU talks
//! to over a vhost-user socket.
//!
//! QEMU on macOS can attach a virtio-fs device (`vhost-user-fs-pci`) but has no
//! server for it: virtiofsd, the usual one, is Linux-only. This is one for
//! macOS. The FUSE protocol server and the macOS passthrough filesystem come
//! from libkrun (see THIRD_PARTY_LICENSES.md); the vhost-user side and the
//! event loop are vmmbox's own.
//!
//! The files taken from libkrun keep their original code style, so the lints
//! that would flag it are silenced on those modules only; vmmbox's own files
//! (`daemon`, `vhost_user`) are linted like the rest of the crate.

use std::sync::atomic::{AtomicU8, Ordering};

/// 0 = errors and warnings only, 1 = also debug. Set by `serve`.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(0);

fn debug_enabled() -> bool {
    LOG_LEVEL.load(Ordering::Relaxed) >= 1
}

// The vendored code logs through the `log` crate's macros. These stand in for
// them, writing to stderr, so no logging framework is needed.
macro_rules! error {
    ($($t:tt)*) => { eprintln!("virtiofs: error: {}", format_args!($($t)*)) };
}
macro_rules! warn {
    ($($t:tt)*) => { eprintln!("virtiofs: warning: {}", format_args!($($t)*)) };
}
#[allow(unused_macros)]
macro_rules! info {
    ($($t:tt)*) => {
        if crate::virtiofs::debug_enabled() { eprintln!("virtiofs: {}", format_args!($($t)*)) }
    };
}
macro_rules! debug {
    ($($t:tt)*) => {
        if crate::virtiofs::debug_enabled() { eprintln!("virtiofs: debug: {}", format_args!($($t)*)) }
    };
}
#[allow(unused_macros)]
macro_rules! trace {
    ($($t:tt)*) => {{ let _ = format_args!($($t)*); }};
}

#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod bindings;
mod daemon;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod descriptor_utils;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod file_traits;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod filesystem;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod fs_utils;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod fuse;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod inode_alloc;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod linux_errno;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod multikey;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod passthrough;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod queue;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod server;
mod vhost_user;
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    unused_macros,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
mod worker_message;

use std::ffi::{FromBytesWithNulError, FromVecWithNulError};
use std::io;

use descriptor_utils::Error as DescriptorError;

pub(crate) type Result<T> = std::result::Result<T, FsError>;

/// Size of the queues the device offers.
pub const QUEUE_SIZE: u16 = 1024;

/// A window of guest-visible memory for mapping file contents directly (DAX).
/// vmmbox does not offer one; the server only needs the type.
// Only named by the vendored server; vmmbox offers no such window.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct VirtioShmRegion {
    pub host_addr: u64,
    pub guest_addr: u64,
    pub size: usize,
}

// The payloads are only ever read through Debug.
#[allow(dead_code)]
#[derive(Debug)]
pub enum FsError {
    /// Failed to decode protocol messages.
    DecodeMessage(io::Error),
    /// Failed to encode protocol messages.
    EncodeMessage(io::Error),
    /// The guest failed to send a required extension.
    MissingExtension,
    /// One or more parameters are missing.
    MissingParameter,
    /// A C string parameter is invalid.
    InvalidCString(FromBytesWithNulError),
    InvalidCString2(FromVecWithNulError),
    /// The `len` field of the header is too small.
    InvalidHeaderLength,
    /// The `size` field of the `SetxattrIn` message does not match the length
    /// of the decoded value.
    InvalidXattrSize((u32, usize)),
    QueueReader(DescriptorError),
    QueueWriter(DescriptorError),
}

pub use daemon::serve;
