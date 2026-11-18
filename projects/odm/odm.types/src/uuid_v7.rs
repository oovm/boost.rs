//! Re-export VOS `uuid()` — Iris hosts must not invent a second generator.

pub use schema_input_vos::uuid::{is_v7, uuid};
