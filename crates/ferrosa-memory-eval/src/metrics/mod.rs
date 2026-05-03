//! Graph quality, personalization, and temporal metrics for evaluation.
//!
//! - `kg_metrics`: Edge precision/recall, microstructure fidelity, dedup
//! - `personalization`: Cross-domain transfer, latent preference, style drift
//! - `temporal_metrics`: Decay curve accuracy, threshold-based forgetting

pub mod kg_metrics;
pub mod personalization;
pub mod temporal_metrics;
