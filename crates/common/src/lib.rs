//! Shared types, errors, capabilities, and telemetry used across the
//! Consistent workspace crates (agents, memory, gateway, tools, etc).

/// Capability and execution-level identifiers shared by permission checks.
pub mod capabilities;
/// Workspace-wide error type.
pub mod error;
/// Home directory, executable lookup, and shell/stdin plumbing that differ by OS.
pub mod platform;
/// Tracing setup and cost/latency tracking.
pub mod telemetry;
/// Core domain types (messages, sessions, tool calls) shared across crates.
pub mod types;
