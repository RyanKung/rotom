#![deny(missing_docs)]
#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![deny(clippy::nursery)]

//! Core library for `rotom`.
//!
//! The library owns provider credential flows, token lifecycle, upstream API
//! clients, HTTP server, and status inspection helpers used by both the CLI
//! binary and any embedding integration.

/// Anthropic-compatible Messages API request, response, and SSE adapters.
pub mod anthropic;
/// Codex upstream request/response adapters and streaming support.
pub mod codex;
/// Local credential storage, persisted runtime config, and time utilities.
pub mod config;
/// Background service installation and lifecycle management.
pub mod daemon;
/// Shared error types and result aliases.
pub mod error;
/// Typed request model for evaluation endpoints.
pub mod evaluation;
/// Logging configuration and helpers.
pub mod logging;
/// Model list resolution from the built-in defaults.
pub mod models;
/// OAuth authorization, token exchange, and callback parsing.
pub mod oauth;
/// OpenAI-compatible public request and response types.
pub mod openai;
/// HTTP server entrypoints and route handlers.
pub mod server;
/// Account and rate-limit status fetchers.
pub mod status;
/// Test-only filesystem helpers shared across unit tests.
#[cfg(test)]
pub(crate) mod testsupport;
/// Shared time formatting helpers for CLI and HTTP output.
pub mod timefmt;
/// Provider credential caching and refresh orchestration.
pub mod token;

/// Shared crate error type.
pub use error::{Error, Result};
