//! Incremental Chat Completions to Responses lifecycle translation.
//!
//! Each chat tool index owns one aggregate and one Responses output slot. Text
//! receives its own slot on its first nonempty delta. Slots and item identifiers
//! remain unchanged in delta events, done events, and the stored final response.

use super::{
    response_finished_lifecycle_events, response_function_call_arguments_delta_event,
    response_function_call_arguments_done_event, response_function_call_item_added_event,
    response_function_call_item_done_event, response_output_text_delta_event,
    response_started_lifecycle_events,
};
use crate::openai::{
    response::{
        ResponseOutputItem, ToolCallDelta, response_function_call_item, response_message_item,
    },
    types::{FunctionCall, ToolCall},
};
use axum::response::sse::Event;
use std::collections::{BTreeMap, btree_map::Entry};

/// One opened function call and the stable Responses slot it owns.
struct StreamedToolCall {
    /// Output position allocated when the call first appears.
    output_index: u32,
    /// Item identifier shared by all lifecycle events and final output.
    item_id: String,
    /// Metadata and arguments accumulated from chat deltas.
    call: ToolCall,
}

/// State for synthesizing Responses events from incremental chat chunks.
pub(super) struct OpenAiResponsesStreamState {
    /// Response identifier used in all emitted lifecycle events.
    response_id: String,
    /// Stable identifier for the optional assistant text output item.
    text_item_id: String,
    /// Sequence number to assign to the next emitted event.
    sequence_number: u64,
    /// Concatenation of emitted assistant text deltas.
    output_text: String,
    /// Calls keyed by their Chat Completions tool index, independent of order.
    tool_calls: BTreeMap<u32, StreamedToolCall>,
    /// Slot allocated to text when its first nonempty delta arrives.
    text_output_index: Option<u32>,
    /// Next unallocated Responses output slot.
    next_output_index: u32,
}

impl OpenAiResponsesStreamState {
    /// Starts after the response-created and response-in-progress events.
    pub(super) fn new(response_id: String) -> Self {
        Self {
            text_item_id: format!("msg_{response_id}"),
            response_id,
            sequence_number: 2,
            output_text: String::new(),
            tool_calls: BTreeMap::new(),
            text_output_index: None,
            next_output_index: 0,
        }
    }

    /// Returns the sequence number for the terminal response event.
    pub(super) const fn sequence_number(&self) -> u64 {
        self.sequence_number
    }

    /// Opens the text item once and forwards each nonempty text delta.
    pub(super) fn text_delta_events(&mut self, text: &str) -> Vec<Event> {
        if text.is_empty() {
            return Vec::new();
        }

        // Lifecycle events produced by this single text transition.
        let mut events = Vec::new();
        // Text keeps its first allocated slot even when tools came first.
        let output_index = *self.text_output_index.get_or_insert_with(|| {
            let index = self.next_output_index;
            self.next_output_index = self.next_output_index.saturating_add(1);
            events.extend(response_started_lifecycle_events(
                self.sequence_number,
                &self.response_id,
                index,
                &self.text_item_id,
            ));
            self.sequence_number = self.sequence_number.saturating_add(2);
            index
        });

        self.output_text.push_str(text);
        events.push(response_output_text_delta_event(
            self.sequence_number,
            &self.response_id,
            output_index,
            &self.text_item_id,
            text,
        ));
        self.sequence_number = self.sequence_number.saturating_add(1);
        events
    }

    /// Opens each call once and forwards argument fragments without completing it.
    pub(super) fn tool_call_events(&mut self, delta: ToolCallDelta) -> Vec<Event> {
        // Metadata appears on the opening delta; later deltas may omit it.
        let ToolCallDelta {
            index,
            id,
            function: FunctionCall { name, arguments },
            ..
        } = delta;
        // Lifecycle events produced by this single tool transition.
        let mut events = Vec::new();
        // Independent calls may interleave, so only their chat index identifies them.
        let tool = match self.tool_calls.entry(index) {
            Entry::Vacant(entry) => {
                let output_index = self.next_output_index;
                self.next_output_index = self.next_output_index.saturating_add(1);
                let tool = entry.insert(StreamedToolCall {
                    output_index,
                    item_id: format!("fc_{}_{index}", self.response_id),
                    call: ToolCall {
                        id,
                        kind: "function".to_owned(),
                        function: FunctionCall {
                            name,
                            arguments: String::new(),
                        },
                    },
                });
                events.push(response_function_call_item_added_event(
                    self.sequence_number,
                    &self.response_id,
                    tool.output_index,
                    &tool.item_id,
                    &tool.call,
                ));
                self.sequence_number = self.sequence_number.saturating_add(1);
                tool
            }
            Entry::Occupied(entry) => entry.into_mut(),
        };
        tool.call.function.arguments.push_str(&arguments);
        if !arguments.is_empty() {
            events.push(response_function_call_arguments_delta_event(
                self.sequence_number,
                &self.response_id,
                tool.output_index,
                &tool.item_id,
                &arguments,
            ));
            self.sequence_number = self.sequence_number.saturating_add(1);
        }
        events
    }

    /// Closes every opened output item once the chat stream reports completion.
    pub(super) fn finished_events(&mut self) -> Vec<Event> {
        // Closing lifecycle events retain the slots assigned by opening events.
        let mut events = Vec::new();
        if let Some(output_index) = self.text_output_index {
            events.extend(response_finished_lifecycle_events(
                self.sequence_number,
                &self.response_id,
                output_index,
                &self.text_item_id,
                &self.output_text,
            ));
            self.sequence_number = self.sequence_number.saturating_add(3);
        }
        for tool in self.tool_calls.values() {
            events.push(response_function_call_arguments_done_event(
                self.sequence_number,
                &self.response_id,
                tool.output_index,
                &tool.item_id,
                &tool.call.function.name,
                &tool.call.function.arguments,
            ));
            events.push(response_function_call_item_done_event(
                self.sequence_number.saturating_add(1),
                &self.response_id,
                tool.output_index,
                &tool.item_id,
                &tool.call,
            ));
            self.sequence_number = self.sequence_number.saturating_add(2);
        }
        events
    }

    /// Moves assembled output into the same order and identifiers seen on the wire.
    pub(super) fn take_completed_output(&mut self) -> Vec<ResponseOutputItem> {
        // Sorting by Responses slot preserves first appearance across text and tools.
        let mut output = BTreeMap::new();
        if let Some(output_index) = self.text_output_index {
            output.insert(
                output_index,
                response_message_item(
                    self.text_item_id.clone(),
                    Some(std::mem::take(&mut self.output_text)),
                ),
            );
        }
        for tool in std::mem::take(&mut self.tool_calls).into_values() {
            output.insert(
                tool.output_index,
                response_function_call_item(tool.item_id, tool.call),
            );
        }
        if output.is_empty() {
            output.insert(0, response_message_item(self.text_item_id.clone(), None));
        }
        output.into_values().collect()
    }
}

#[cfg(test)]
mod tests;
