//! Precompiled binaries delivered with Get Latest, in the style of UnrealGameSync.
//!
//! A repository opts in with `.g4v/binaries.toml` (see [`config`]). Its CI publishes one build per *key* as a
//! GitHub release and records, per commit, a JSON note under a notes ref (`refs/notes/binaries`) saying whether the
//! build for that commit is `building`, `ready`, `failed` or `superseded`. Get Latest then moves the workspace to
//! the newest commit whose build is ready — never further — and installs that build in the same transaction, so
//! nobody is ever handed content that needs code they cannot compile.
//!
//! g4v knows nothing about any particular engine or project. Everything project-specific lives in the config file,
//! the notes CI writes and the manifest inside each release.
//!
//! The pieces, all UI-free and testable:
//! * [`config`]  — parse and validate `.g4v/binaries.toml`
//! * [`paths`]   — the only way a repo-provided path becomes a file system path
//! * [`notes`]   — fetch and read the per-commit build notes
//! * [`plan`]    — the pure decision: where Get Latest should take this workspace
//! * [`source`]  — where release assets come from (GitHub, or a local folder for tests and mirrors)
//! * [`install`] — download, verify, stage and swap files in, with a crash-safe transaction and rollback
//! * [`trust`]   — the user's per-repository decision to let a repo install files and run its commands
//! * [`sync`]    — Get Latest, put together
//! * [`builder`] — a build running on this machine (the loading bar on CI runners)

pub mod builder;
pub mod config;
pub mod install;
pub mod notes;
pub mod paths;
pub mod plan;
pub mod source;
pub mod sync;
pub mod trust;

pub use config::{BinariesConfig, LoadedConfig};
pub use sync::{BinStatus, BinSyncOutcome};

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Strips a UTF-8 byte order mark (Windows PowerShell 5.1 writes one).
pub(crate) fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}
