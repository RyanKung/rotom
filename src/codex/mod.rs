/// Codex API client and transport helpers.
pub mod client;
/// Converts OpenAI-style chat payloads into Codex request bodies.
pub mod convert;
/// Aggregates streamed Codex events into chat output state.
pub mod events;
mod kiro;
mod responses_input;
/// Parses server-sent events emitted by Codex streaming responses.
pub mod sse;
mod upstream;
