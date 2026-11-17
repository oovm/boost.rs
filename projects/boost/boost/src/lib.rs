//! Unified entry for the boost standard-library enhancement family.
//!
//! Enable capabilities through crate features. Each module re-exports the
//! corresponding `boost-*` crate without copying types or adding global state.

#![deny(missing_docs)]

#[cfg(feature = "collection")]
pub mod collection {
    //! Rule-shaped collections with contiguous storage and checked shapes.
    pub use boost_collection::*;
    pub use boost_collection_macro::facade_array as array;
}

#[cfg(feature = "console")]
pub mod console {
    //! Human-visible console output and log subscribers.
    pub use boost_console::*;
    pub use boost_harness::log::console::*;
}

#[cfg(feature = "crypto")]
pub mod crypto {
    //! Cryptographic primitives and secret material lifetimes.
    pub use boost_crypto::*;
}

#[cfg(feature = "diagnostic")]
pub mod diagnostic {
    //! Structured diagnostic data model and projections.
    pub use boost_harness::diagnostic::*;
}

#[cfg(feature = "fmt")]
pub mod fmt {
    //! Text styling, width, and layout.
    pub use boost_console::fmt::*;
}

#[cfg(feature = "fs")]
pub mod fs {
    //! File system utilities.
    pub use boost_fs::*;
}

#[cfg(feature = "log")]
pub mod log {
    //! Structured log events, spans, filters, and sinks.
    pub use boost_harness::log::*;
}

#[cfg(feature = "random")]
pub mod random {
    //! Random sources, sampling, and distributions.
    pub use boost_random::*;
}

#[cfg(feature = "source")]
pub mod source {
    //! Source identity, snapshots, bytes, and coordinates.
    pub use boost_source::*;
}

#[cfg(feature = "terminal")]
pub mod terminal {
    //! Terminal device capabilities.
    pub use boost_terminal::*;
}

#[cfg(feature = "test")]
pub mod test {
    //! Correctness tests, cases, fixtures, and assertions.
    pub use boost_harness::test::*;
    pub use boost_harness_macro::{facade_case as case, facade_fixture as fixture, facade_test as test};
}

#[cfg(feature = "benchmark")]
pub mod benchmark {
    //! Performance measurement with explicit samples and structured reports.
    pub use boost_harness::benchmark::*;
}
