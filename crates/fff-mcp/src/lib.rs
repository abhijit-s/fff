// Public API surface for integration tests. Not part of the MCP server binary.
#[cfg(unix)]
pub mod client;
#[cfg(unix)]
pub mod pool;
#[cfg(unix)]
mod recovery;
pub mod registry;
