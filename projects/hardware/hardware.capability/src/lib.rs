//! Backend capability / feature flags.
#![warn(missing_docs)]

mod degrade;
mod feature;
mod report;
mod set;

pub use degrade::{
    DescriptorStrategy, PipelineStrategy, SyncStrategy, WiredStrategies, pick_descriptor_strategy, pick_pipeline_strategy, pick_sync_strategy,
};
pub use feature::Feature;
pub use report::{BackendCapabilities, CapabilityReport};
pub use set::FeatureSet;
