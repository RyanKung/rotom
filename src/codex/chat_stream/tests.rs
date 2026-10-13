//! Regression witnesses for the append-only tool argument state machine.

use super::{ToolStream, ToolStreamError, chat_chunks};
use crate::openai::response::ToolCallDelta;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

/// Builds one complete function item with distinct item and public call IDs.
fn function_item(arguments: &str) -> Value {
    json!({
        "type": "function_call",
        "id": "fc_lookup",
        "call_id": "call_lookup",
        "name": "lookup",
        "arguments": arguments
    })
}

/// Introduces a metadata-only call after unrelated upstream output slots.
fn added_event() -> Value {
    json!({
        "type": "response.output_item.added",
        "output_index": 5,
        "item": {
            "type": "function_call",
            "id": "fc_lookup",
            "call_id": "call_lookup",
            "name": "lookup"
        }
    })
}

/// Completes the fixed fixture call with an authoritative argument snapshot.
fn done_event(arguments: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "output_index": 5,
        "item": function_item(arguments)
    })
}

/// Wraps final output items in the terminal Responses event.
fn terminal_event(output: &[Value]) -> Value {
    json!({
        "type": "response.completed",
        "response": {"status": "completed", "output": output}
    })
}

/// A metadata-only introduction is sufficient for a done-only argument backfill.
#[test]
fn item_completion_backfills_metadata_only_call_once() -> Result<(), ToolStreamError> {
    // The registry starts without an introduced call or reserved Chat index.
    let mut tools = ToolStream::default();
    // The metadata event reserves one dense Chat index despite upstream index 5.
    let introduction = tools.apply(&added_event())?;
    assert_eq!(introduction.len(), 1);
    assert!(introduction.first().is_some_and(|delta| {
        delta.index == 0
            && delta.id == "call_lookup"
            && delta.kind == "function"
            && delta.function.name == "lookup"
            && delta.function.arguments.is_empty()
    }));

    // The authoritative snapshot contributes arguments without repeated metadata.
    let completion = tools.apply(&done_event(r#"{"q":"rust"}"#))?;
    assert_eq!(completion.len(), 1);
    assert!(completion.first().is_some_and(|delta| {
        delta.index == 0
            && delta.id.is_empty()
            && delta.kind.is_empty()
            && delta.function.name.is_empty()
            && delta.function.arguments == r#"{"q":"rust"}"#
    }));
    assert_eq!(
        tools.apply(&done_event(r#"{"q":"rust"}"#))?,
        Vec::<ToolCallDelta>::new()
    );
    tools.ensure_complete()
}

/// A final snapshot contributes only the suffix absent from streamed fragments.
#[test]
fn final_snapshot_preserves_the_streamed_argument_prefix() -> Result<(), ToolStreamError> {
    // Each emitted fragment must be retained exactly once in the client prefix.
    let mut tools = ToolStream::default();
    tools.apply(&added_event())?;
    for fragment in [r#"{"q":""#, "rust"] {
        // The Responses output index routes fragments to their introduced call.
        let delta = tools.apply(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 5,
            "delta": fragment
        }))?;
        assert_eq!(delta.len(), 1);
        assert!(
            delta
                .first()
                .is_some_and(|value| value.function.arguments == fragment)
        );
    }

    // Only the closing JSON bytes remain after the previously delivered prefix.
    let completion = tools.apply(&done_event(r#"{"q":"rust"}"#))?;
    assert_eq!(completion.len(), 1);
    assert!(
        completion
            .first()
            .is_some_and(|delta| delta.function.arguments == "\"}")
    );
    assert_eq!(
        tools.apply(&terminal_event(&[function_item(r#"{"q":"rust"}"#)]))?,
        Vec::<ToolCallDelta>::new()
    );
    tools.ensure_complete()
}

/// Argument events may route by item ID without exposing the upstream index.
#[test]
fn item_identity_routes_argument_delta_and_done_without_output_index() -> Result<(), ToolStreamError>
{
    // Item identity differs from the call ID consumed by Chat tool clients.
    let mut tools = ToolStream::default();
    tools.apply(&added_event())?;
    // This fragment has only the Responses item identity used by native events.
    let delta = tools.apply(&json!({
        "type": "response.function_call_arguments.delta",
        "item_id": "fc_lookup",
        "delta": "{"
    }))?;
    assert!(
        delta
            .first()
            .is_some_and(|value| value.index == 0 && value.function.arguments == "{")
    );
    // The done event uses the same item ID and repairs the final missing suffix.
    let completion = tools.apply(&json!({
        "type": "response.function_call_arguments.done",
        "item_id": "fc_lookup",
        "arguments": r#"{"q":1}"#
    }))?;
    assert_eq!(completion.len(), 1);
    assert!(
        completion
            .first()
            .is_some_and(|value| value.function.arguments == r#""q":1}"#)
    );
    tools.ensure_complete()
}

/// Repeated final snapshots do not replay a done-only argument payload.
#[test]
fn arguments_done_backfill_is_deduplicated_by_item_and_terminal_snapshots()
-> Result<(), ToolStreamError> {
    // A call can finish without any argument delta event from its provider.
    let mut tools = ToolStream::default();
    tools.apply(&added_event())?;
    // Argument completion is the first authoritative full payload.
    let completion = tools.apply(&json!({
        "type": "response.function_call_arguments.done",
        "output_index": 5,
        "arguments": r#"{"q":2}"#
    }))?;
    assert_eq!(completion.len(), 1);
    assert!(
        completion
            .first()
            .is_some_and(|value| value.function.arguments == r#"{"q":2}"#)
    );
    assert_eq!(
        tools.apply(&done_event(r#"{"q":2}"#))?,
        Vec::<ToolCallDelta>::new()
    );
    assert_eq!(
        tools.apply(&terminal_event(&[function_item(r#"{"q":2}"#)]))?,
        Vec::<ToolCallDelta>::new()
    );
    tools.ensure_complete()
}

/// Omitted final arguments retain prior bytes with or without argument completion.
#[test]
fn metadata_only_final_snapshots_preserve_previously_streamed_arguments()
-> Result<(), ToolStreamError> {
    for arguments_done in [false, true] {
        // Either the argument event or item completion may first finalize a call.
        let mut tools = ToolStream::default();
        tools.apply(&added_event())?;
        tools.apply(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 5,
            "delta": r#"{"q":1}"#
        }))?;
        if arguments_done {
            assert_eq!(
                tools.apply(&json!({
                    "type": "response.function_call_arguments.done",
                    "output_index": 5,
                    "arguments": r#"{"q":1}"#
                }))?,
                Vec::<ToolCallDelta>::new()
            );
        }
        // Some providers repeat only metadata in item and terminal snapshots.
        let metadata = json!({
            "type": "function_call", "id": "fc_lookup", "call_id": "call_lookup",
            "name": "lookup"
        });
        assert_eq!(
            tools.apply(&json!({
                "type": "response.output_item.done", "output_index": 5, "item": &metadata
            }))?,
            Vec::<ToolCallDelta>::new()
        );
        assert_eq!(
            tools.apply(&terminal_event(&[metadata]))?,
            Vec::<ToolCallDelta>::new()
        );
        assert!(
            tools
                .calls
                .first()
                .is_some_and(|call| call.arguments == r#"{"q":1}"#)
        );
        tools.ensure_complete()?;
    }
    Ok(())
}

/// Terminal-only calls receive dense Chat indexes and omit unrelated item kinds.
#[test]
fn final_only_calls_ignore_non_function_items_and_keep_dense_indexes() -> Result<(), ToolStreamError>
{
    // No streaming introductions are required when the terminal embeds output.
    let mut tools = ToolStream::default();
    // Reasoning, messages, and hosted tools do not consume Chat tool indexes.
    let deltas = tools.apply(&terminal_event(&[
        json!({"type": "reasoning", "id": "rs_1", "summary": []}),
        function_item(r#"{"q":3}"#),
        json!({"type": "message", "id": "msg_1", "content": []}),
        json!({"type": "web_search_call", "id": "ws_1", "status": "completed"}),
        json!({
            "type": "function_call", "id": "fc_second", "call_id": "call_second",
            "name": "clock", "arguments": "{}"
        }),
    ]))?;
    assert_eq!(deltas.len(), 2);
    assert_eq!(
        deltas.iter().map(|delta| delta.index).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        deltas
            .iter()
            .map(|delta| delta.id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_lookup", "call_second"]
    );
    assert_eq!(
        deltas
            .iter()
            .map(|delta| delta.function.arguments.as_str())
            .collect::<Vec<_>>(),
        vec![r#"{"q":3}"#, "{}"]
    );
    tools.ensure_complete()
}

/// A conflicting final snapshot fails without corrupting the accepted prefix.
#[test]
fn conflicting_snapshot_is_rejected_and_leaves_the_call_unfinished() -> Result<(), ToolStreamError>
{
    // The client has already received a prefix that cannot be revised in place.
    let mut tools = ToolStream::default();
    tools.apply(&added_event())?;
    tools.apply(&json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 5,
        "delta": r#"{"q":1"#
    }))?;
    assert!(matches!(
        tools.apply(&done_event(r#"{"q":2}"#)),
        Err(ToolStreamError::ConflictingArguments)
    ));
    assert!(matches!(
        tools.ensure_complete(),
        Err(ToolStreamError::UnfinishedCalls)
    ));
    // A subsequent matching snapshot witnesses that rejection did not mutate it.
    let completion = tools.apply(&done_event(r#"{"q":1}"#))?;
    assert!(
        completion
            .first()
            .is_some_and(|delta| delta.function.arguments == "}")
    );
    tools.ensure_complete()
}

/// Missing introduction metadata cannot fabricate a call, but finals recover it.
#[test]
fn orphan_argument_events_wait_for_a_complete_identified_item() -> Result<(), ToolStreamError> {
    // Without metadata, no Chat call identity or function name is available.
    let mut tools = ToolStream::default();
    for event in [
        json!({"type": "response.function_call_arguments.delta", "item_id": "fc_lookup", "delta": "{"}),
        json!({"type": "response.function_call_arguments.done", "item_id": "fc_lookup", "arguments": "{}"}),
    ] {
        assert_eq!(tools.apply(&event)?, Vec::<ToolCallDelta>::new());
    }
    // A final-only item supplies the metadata and complete payload atomically.
    let recovered = tools.apply(&done_event("{}"))?;
    assert_eq!(recovered.len(), 1);
    assert!(recovered.first().is_some_and(|delta| {
        delta.index == 0 && delta.id == "call_lookup" && delta.function.arguments == "{}"
    }));
    tools.ensure_complete()
}

/// Both bare EOF and an empty terminal reject an unfinished call without success.
#[tokio::test]
async fn unfinished_calls_emit_an_error_instead_of_a_successful_finish() {
    for terminal in [None, Some(terminal_event(&[]))] {
        // The same incomplete call is exercised with both forms of stream ending.
        let events = std::iter::once(added_event()).chain(terminal).map(Ok);
        // The public converter must expose the domain failure to its SSE adapter.
        let chunks: Vec<_> = chat_chunks(
            stream::iter(events),
            "chat_1".to_owned(),
            1,
            "test".to_owned(),
        )
        .collect()
        .await;
        assert!(chunks.last().is_some_and(|result| {
            result
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string().contains("unfinished tool calls"))
        }));
        assert!(
            chunks
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .flat_map(|chunk| &chunk.choices)
                .all(|choice| choice.finish_reason.is_none())
        );
    }
}
