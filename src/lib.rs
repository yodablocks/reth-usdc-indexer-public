pub mod db;
pub mod types;

/// The Reth ExEx integration. Requires a full Reth dependency tree.
///
/// Gated behind the `exex` feature so that the storage layer, the reorg
/// tests, and the benchmark can be built and run without compiling Reth.
#[cfg(feature = "exex")]
pub mod exex;

pub use db::{Database, TransferRecord};

#[cfg(feature = "exex")]
pub use exex::UsdcIndexer;
