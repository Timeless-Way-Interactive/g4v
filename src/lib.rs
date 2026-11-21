//! g4v — a friendly, Perforce-P4V-style visual client for Git.
//!
//! The crate is split into a UI-free backend (`git`, `changelists`,
//! `settings`, `worker`) that is fully testable, and the egui front end (`ui`).

pub mod binaries;
pub mod changelists;
pub mod git;
pub mod settings;
pub mod ui;
pub mod worker;
