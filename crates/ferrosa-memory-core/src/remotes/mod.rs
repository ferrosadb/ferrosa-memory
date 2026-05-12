//! Module: Remote teacher/learner transfer domain surface.
//! Correctness: Correct when packet types round-trip, remote storage remains tenant-scoped, and policy/import rows are represented without lossy JSON call sites.
//! Last revised: 2026-05-12
//! Last changed: Added Packet C remote policy evaluation module.

pub mod policy;
pub mod types;
