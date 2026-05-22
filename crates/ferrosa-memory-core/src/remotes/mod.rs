//! Module: Remote teacher/learner transfer domain surface.
//! Correctness: Correct when packet types round-trip, remote storage remains tenant-scoped, and policy/import rows are represented without lossy JSON call sites.
//! Last revised: 2026-05-12
//! Last changed: Added Packet G detail refs and Packet H feedback modules.

pub mod applicability;
pub mod archive;
pub mod detail;
pub mod feedback;
pub mod policy;
pub mod pull;
pub mod safety;
pub mod teach;
pub mod types;
