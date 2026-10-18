//! Server discovery for the ContextVM protocol.
//!
//! Discover MCP servers and their capabilities (tools, resources, prompts)
//! published as Nostr events on relays.
//!
//! # Example
//!
//! ```rust,no_run
//! use contextvm_sdk::discovery;
//! use contextvm_sdk::signer;
//!
//! # async fn example() -> contextvm_sdk::Result<()> {
//! let keys = signer::generate();
//! let relay_pool = contextvm_sdk::RelayPool::new(keys).await?;
//! let relays = vec!["wss://relay.damus.io".to_string()];
//! relay_pool.connect(&relays).await?;
//! let client = relay_pool.client();
//!
//! let servers = discovery::discover_servers(client, &relays).await?;
//! for server in &servers {
//!     println!("Found server: {} ({:?})", server.pubkey, server.server_info.name);
//!     let tools = discovery::discover_tools(client, &server.pubkey_parsed, &relays).await?;
//!     println!("  Tools: {:?}", tools);
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::ServerInfo;

/// A discovered server announcement.
#[derive(Debug, Clone)]
pub struct ServerAnnouncement {
    /// Server public key (hex).
    pub pubkey: String,
    /// Parsed public key.
    pub pubkey_parsed: PublicKey,
    /// Server information extracted from the announcement content.
    pub server_info: ServerInfo,
    /// The Nostr event ID of the announcement.
    pub event_id: EventId,
    /// When the announcement was created.
    pub created_at: Timestamp,
    /// MCP protocol version (present when content is a full `InitializeResult`).
    pub protocol_version: Option<String>,
    /// Server capabilities (present when content is a full `InitializeResult`).
    pub capabilities: Option<serde_json::Value>,
    /// Human-readable instructions (present when content is a full `InitializeResult`).
    pub instructions: Option<String>,
}

/// Discover MCP servers by fetching kind 11316 announcement events from relays.
pub async fn discover_servers(
    client: &Arc<Client>,
    _relay_urls: &[String],
) -> Result<Vec<ServerAnnouncement>> {
    let filter = Filter::new().kind(Kind::Custom(SERVER_ANNOUNCEMENT_KIND));

    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;

    let mut announcements = Vec::new();
    for event in events {
        let (server_info, protocol_version, capabilities, instructions) =
            parse_announcement_content(&event.content);
        announcements.push(ServerAnnouncement {
            pubkey: event.pubkey.to_hex(),
            pubkey_parsed: event.pubkey,
            server_info,
            event_id: event.id,
            created_at: event.created_at,
            protocol_version,
            capabilities,
            instructions,
        });
    }

    Ok(announcements)
}

/// Discover tools published by a specific server (kind 11317).
pub async fn discover_tools(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(client, server_pubkey, TOOLS_LIST_KIND, "tools").await
}

/// Discover resources published by a specific server (kind 11318).
pub async fn discover_resources(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(client, server_pubkey, RESOURCES_LIST_KIND, "resources").await
}

/// Discover prompts published by a specific server (kind 11320).
pub async fn discover_prompts(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(client, server_pubkey, PROMPTS_LIST_KIND, "prompts").await
}

/// Discover resource templates published by a specific server (kind 11319).
pub async fn discover_resource_templates(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    _relay_urls: &[String],
) -> Result<Vec<serde_json::Value>> {
    fetch_list(
        client,
        server_pubkey,
        RESOURCETEMPLATES_LIST_KIND,
        "resourceTemplates",
    )
    .await
}

/// Discover tools and parse them into rmcp typed descriptors.
#[cfg(feature = "rmcp")]
pub async fn discover_tools_typed(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    relay_urls: &[String],
) -> Result<Vec<rmcp::model::Tool>> {
    let raw = discover_tools(client, server_pubkey, relay_urls).await?;
    parse_typed_list(raw)
}

/// Discover resources and parse them into rmcp typed descriptors.
#[cfg(feature = "rmcp")]
pub async fn discover_resources_typed(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    relay_urls: &[String],
) -> Result<Vec<rmcp::model::Resource>> {
    let raw = discover_resources(client, server_pubkey, relay_urls).await?;
    parse_typed_list(raw)
}

/// Discover prompts and parse them into rmcp typed descriptors.
#[cfg(feature = "rmcp")]
pub async fn discover_prompts_typed(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    relay_urls: &[String],
) -> Result<Vec<rmcp::model::Prompt>> {
    let raw = discover_prompts(client, server_pubkey, relay_urls).await?;
    parse_typed_list(raw)
}

/// Discover resource templates and parse them into rmcp typed descriptors.
#[cfg(feature = "rmcp")]
pub async fn discover_resource_templates_typed(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    relay_urls: &[String],
) -> Result<Vec<rmcp::model::ResourceTemplate>> {
    let raw = discover_resource_templates(client, server_pubkey, relay_urls).await?;
    parse_typed_list(raw)
}

// ── Internal ────────────────────────────────────────────────────────

/// Parse kind 11316 event content, supporting two formats:
///
/// - **New (InitializeResult):** `{ "protocolVersion": "…", "capabilities": {…},
///   "serverInfo": {…}, "instructions": "…" }` — used when the server publishes
///   the full MCP InitializeResult as content.
/// - **Legacy (ServerInfo):** `{ "name": "…", "version": "…", … }` — the original
///   rs-sdk format where content is just `ServerInfo`.
fn parse_announcement_content(
    content: &str,
) -> (
    ServerInfo,
    Option<String>,
    Option<serde_json::Value>,
    Option<String>,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return (ServerInfo::default(), None, None, None);
    };

    // Detect new format by the presence of "protocolVersion" (camelCase from rmcp).
    if value.get("protocolVersion").is_some() {
        let server_info = value
            .get("serverInfo")
            .map(server_info_from_implementation)
            .unwrap_or_default();
        let protocol_version = value
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .map(String::from);
        let capabilities = value.get("capabilities").cloned();
        let instructions = value
            .get("instructions")
            .and_then(|v| v.as_str())
            .map(String::from);
        (server_info, protocol_version, capabilities, instructions)
    } else {
        // Legacy: content is a flat ServerInfo object.
        let server_info = serde_json::from_value::<ServerInfo>(value).unwrap_or_default();
        (server_info, None, None, None)
    }
}

/// Map an rmcp `Implementation` JSON object to our `ServerInfo`.
///
/// Field mapping: `name`→`name`, `version`→`version`,
/// `websiteUrl`→`website`, `description`→`about`. The `picture` field has no
/// equivalent in `Implementation` so it is left `None`.
fn server_info_from_implementation(val: &serde_json::Value) -> ServerInfo {
    ServerInfo {
        name: val.get("name").and_then(|v| v.as_str()).map(String::from),
        version: val
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from),
        website: val
            .get("websiteUrl")
            .and_then(|v| v.as_str())
            .map(String::from),
        about: val
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from),
        picture: None,
    }
}

async fn fetch_list(
    client: &Arc<Client>,
    server_pubkey: &PublicKey,
    kind: u16,
    list_key: &str,
) -> Result<Vec<serde_json::Value>> {
    let filter = Filter::new()
        .kind(Kind::Custom(kind))
        .author(*server_pubkey);

    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;

    // Take the most recent event
    let event = match events.into_iter().next() {
        Some(e) => e,
        None => return Ok(Vec::new()),
    };

    let parsed: serde_json::Value =
        serde_json::from_str(&event.content).map_err(|e| Error::Other(e.to_string()))?;

    Ok(parsed
        .get(list_key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

#[cfg(feature = "rmcp")]
fn parse_typed_list<T>(raw: Vec<serde_json::Value>) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut parsed = Vec::new();
    for item in raw {
        let value = serde_json::from_value(item)
            .map_err(|e| Error::Other(format!("Failed to parse typed discovery item: {e}")))?;
        parsed.push(value);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ServerInfo;

    #[test]
    fn test_server_info_serialization() {
        let info = ServerInfo {
            name: Some("Test Server".to_string()),
            version: Some("1.0.0".to_string()),
            about: Some("A test MCP server".to_string()),
            website: Some("https://example.com".to_string()),
            picture: Some("https://example.com/pic.png".to_string()),
        };

        let json = serde_json::to_string(&info).unwrap();
        let parsed: ServerInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, Some("Test Server".to_string()));
        assert_eq!(parsed.version, Some("1.0.0".to_string()));
        assert_eq!(parsed.about, Some("A test MCP server".to_string()));
        assert_eq!(parsed.website, Some("https://example.com".to_string()));
        assert_eq!(
            parsed.picture,
            Some("https://example.com/pic.png".to_string())
        );
    }

    #[test]
    fn test_server_info_default() {
        let info = ServerInfo::default();
        assert!(info.name.is_none());
        assert!(info.version.is_none());
        assert!(info.about.is_none());
        assert!(info.website.is_none());
        assert!(info.picture.is_none());
    }

    #[test]
    fn test_server_info_partial_serialization() {
        let info = ServerInfo {
            name: Some("Minimal".to_string()),
            ..Default::default()
        };

        let json = serde_json::to_string(&info).unwrap();
        // Optional fields should be skipped
        assert!(!json.contains("version"));
        assert!(!json.contains("about"));
        assert!(json.contains("Minimal"));
    }

    #[test]
    fn test_server_info_deserialization_from_empty() {
        let info: ServerInfo = serde_json::from_str("{}").unwrap();
        assert!(info.name.is_none());
    }


    #[test]
    fn test_parse_announcement_content_legacy_format() {
        let content = r#"{"name":"Legacy Server","version":"0.1.0","about":"Old format"}"#;
        let (info, pv, caps, instr) = super::parse_announcement_content(content);
        assert_eq!(info.name.as_deref(), Some("Legacy Server"));
        assert_eq!(info.version.as_deref(), Some("0.1.0"));
        assert_eq!(info.about.as_deref(), Some("Old format"));
        assert!(pv.is_none());
        assert!(caps.is_none());
        assert!(instr.is_none());
    }

    #[test]
    fn test_parse_announcement_content_initialize_result_format() {
        let content = r#"{
            "protocolVersion": "2025-03-26",
            "capabilities": {
                "tools": { "listChanged": true },
                "resources": { "subscribe": false, "listChanged": false }
            },
            "serverInfo": {
                "name": "NewServer",
                "version": "2.0.0",
                "description": "Full InitializeResult",
                "websiteUrl": "https://example.com"
            },
            "instructions": "Use tool X for Y"
        }"#;
        let (info, pv, caps, instr) = super::parse_announcement_content(content);

        assert_eq!(info.name.as_deref(), Some("NewServer"));
        assert_eq!(info.version.as_deref(), Some("2.0.0"));
        assert_eq!(info.about.as_deref(), Some("Full InitializeResult"));
        assert_eq!(info.website.as_deref(), Some("https://example.com"));
        assert!(info.picture.is_none());

        assert_eq!(pv.as_deref(), Some("2025-03-26"));
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert!(caps.get("tools").is_some());
        assert_eq!(instr.as_deref(), Some("Use tool X for Y"));
    }

    #[test]
    fn test_parse_announcement_content_invalid_json() {
        let (info, pv, caps, instr) = super::parse_announcement_content("not json");
        assert!(info.name.is_none());
        assert!(pv.is_none());
        assert!(caps.is_none());
        assert!(instr.is_none());
    }

    #[test]
    fn test_parse_announcement_content_empty_object() {
        let (info, pv, caps, instr) = super::parse_announcement_content("{}");
        assert!(info.name.is_none());
        assert!(pv.is_none());
        assert!(caps.is_none());
        assert!(instr.is_none());
    }

    #[test]
    fn test_server_info_from_implementation() {
        let val = serde_json::json!({
            "name": "TestImpl",
            "version": "3.0",
            "title": "Fancy Title",
            "description": "Impl description",
            "websiteUrl": "https://impl.example.com"
        });
        let info = super::server_info_from_implementation(&val);
        assert_eq!(info.name.as_deref(), Some("TestImpl"));
        assert_eq!(info.version.as_deref(), Some("3.0"));
        assert_eq!(info.website.as_deref(), Some("https://impl.example.com"));
        assert_eq!(info.about.as_deref(), Some("Impl description"));
        assert!(info.picture.is_none());
    }
}
