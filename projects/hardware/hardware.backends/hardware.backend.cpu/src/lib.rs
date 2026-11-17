//! Host CPU reference backend (`Copy` / `Fill` / `MatMul`).
#![warn(missing_docs)]

mod backend;
mod external;
mod params;
mod raster_tri;
mod store;

pub use backend::{CpuBackend, ProgramExecutionReport};
pub use external::HostExternalBuffer;
pub use store::{HostStore, SharedHostStore, shared_store};
