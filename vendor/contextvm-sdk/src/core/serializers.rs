//! Serialization utilities for converting between MCP messages and Nostr events.

use nostr_sdk::prelude::*;

use crate::core::types::JsonRpcMessage;

/// Convert an MCP message to a Nostr event template (unsigned).
///
/// The message is JSON-serialized into the event's `content` field.
pub fn mcp_to_nostr_event(
    message: &JsonRpcMessage,
    kind: u16,
    tags: Vec<Tag>,
) -> Result<EventBuilder, serde_json::Error> {
    let content = serde_json::to_string(message)?;
    Ok(EventBuilder::new(Kind::Custom(kind), content).tags(tags))
}

/// Deserialize a Nostr event's content into an MCP message.
///
/// Returns `None` if the content is not valid JSON or not a valid JSON-RPC message.
pub fn nostr_event_to_mcp_message(content: &str) -> Option<JsonRpcMessage> {
    serde_json::from_str(content).ok()
}

/// Extract a tag value from a Nostr event's tags.
pub fn get_tag_value(tags: &Tags, name: &str) -> Option<String> {
    tags.iter().find_map(|tag| {
        let vec = tag.clone().to_vec();
        if vec.first().map(|s| s.as_str()) == Some(name) {
            vec.get(1).cloned()
        } else {
            None
        }
    })
}

/// Extract a tag value from a slice of tags.
pub fn get_tag_value_from_slice(tags: &[Tag], name: &str) -> Option<String> {
    tags.iter().find_map(|tag| {
        let vec = tag.clone().to_vec();
        if vec.first().map(|s| s.as_str()) == Some(name) {
            vec.get(1).cloned()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{JsonRpcMessage, JsonRpcRequest};
    use serde_json::Value;

    #[test]
    fn test_roundtrip() {
        let msg = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Value::Number(1.into()),
            method: "tools/list".to_string(),
            params: None,
        });

        let content = serde_json::to_string(&msg).unwrap();
        let parsed = nostr_event_to_mcp_message(&content).unwrap();
        assert!(parsed.is_request());
        assert_eq!(parsed.method(), Some("tools/list"));
    }

    #[test]
    fn test_invalid_json() {
        assert!(nostr_event_to_mcp_message("not json").is_none());
    }
}
