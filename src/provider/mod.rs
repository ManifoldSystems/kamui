use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub mod openai;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// An image attached to a message, carried as base64 so it is provider-independent.
#[derive(Clone, Debug)]
pub struct ImageAttachment {
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    pub data: String,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Images attached to this message. Only meaningful on user messages.
    pub images: Vec<ImageAttachment>,
    /// Tool calls requested by an assistant turn. Empty for ordinary messages.
    pub tool_calls: Vec<ToolCall>,
    /// The call this message answers. Set only on tool-result messages.
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    /// A user turn carrying attached images alongside its text.
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant turn that requests one or more tool calls.
    pub fn tool_request(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// The result of executing a tool, fed back to the model.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub fn role_name(&self) -> &'static str {
        match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn from_parts(role: &str, content: String) -> Result<Self> {
        let role = match role {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => anyhow::bail!("unknown message role: {role}"),
        };

        Ok(Self {
            role,
            content,
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        })
    }
}

/// A provider-independent tool the model may call.
#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the tool's parameters.
    pub parameters: Value,
}

/// A tool invocation requested by the model. Serializable so it can be persisted with its message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments exactly as produced by the model.
    pub arguments: String,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Sticky session for providers that require it (Orvix Coding Plan).
    pub session_id: Option<String>,
}

#[derive(Debug)]
pub struct ChatResponse {
    pub content: String,
    // Parsed and tested now; the agent loop that consumes these lands in the next Phase 3 increment.
    #[allow(dead_code)]
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: String,
}

#[derive(Debug)]
pub enum StreamEvent {
    Delta(String),
    /// Provider reasoning/thinking text. Display-only; never replayed to the model.
    Reasoning(String),
    Done {
        usage: Usage,
        finish_reason: String,
        tool_calls: Vec<ToolCall>,
    },
}

#[derive(Debug, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
}

fn parse_u64(value: &Value) -> u64 {
    match value {
        Value::Number(number) => number.as_u64().unwrap_or(0),
        Value::String(text) => text.parse().unwrap_or(0),
        _ => 0,
    }
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let map = match &value {
            Value::Object(map) => map,
            _ => return Ok(Self::default()),
        };
        let prompt_tokens = map.get("prompt_tokens").map(parse_u64).unwrap_or(0);
        let completion_tokens = map.get("completion_tokens").map(parse_u64).unwrap_or(0);
        let total_tokens = map.get("total_tokens").map(parse_u64).unwrap_or(0);
        let cached_tokens = map
            .get("prompt_tokens_details")
            .and_then(|details| match details {
                Value::Object(details_map) => details_map.get("cached_tokens").map(parse_u64),
                _ => None,
            })
            .or_else(|| map.get("cache_read_input_tokens").map(parse_u64))
            // Orvix / some OpenAI-compat hosts put the cache hit count at the top level.
            .or_else(|| map.get("cached_prompt_tokens").map(parse_u64))
            .or_else(|| map.get("cached_tokens").map(parse_u64))
            .unwrap_or(0);
        Ok(Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
        })
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::UnboundedReceiver<Result<StreamEvent>>>;

    /// Embed a batch of texts for `/index`/`search_code`, returned in the same order as `input`.
    /// Only called when the active profile's `config::Profile::embedding_model` is configured.
    async fn embed(&self, model: &str, input: Vec<String>) -> Result<Vec<Vec<f32>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_round_trips_known_roles() {
        for role in ["system", "user", "assistant", "tool"] {
            let message = Message::from_parts(role, "body".to_string()).unwrap();
            assert_eq!(message.role_name(), role);
            assert_eq!(message.content, "body");
            assert!(message.tool_calls.is_empty());
            assert!(message.tool_call_id.is_none());
        }
    }

    #[test]
    fn from_parts_rejects_unknown_roles() {
        assert!(Message::from_parts("wizard", "body".to_string()).is_err());
    }

    #[test]
    fn tool_request_carries_calls_on_an_assistant_turn() {
        let message = Message::tool_request(
            "",
            vec![ToolCall {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            }],
        );

        assert_eq!(message.role_name(), "assistant");
        assert_eq!(message.tool_calls.len(), 1);
        assert!(message.tool_call_id.is_none());
    }

    #[test]
    fn tool_result_records_the_answered_call() {
        let message = Message::tool_result("c1", "file body");

        assert_eq!(message.role_name(), "tool");
        assert_eq!(message.tool_call_id.as_deref(), Some("c1"));
        assert!(message.tool_calls.is_empty());
    }

    #[test]
    fn usage_deserializes_openai_cached_tokens() {
        let usage: Usage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":42}}"#,
        )
        .unwrap();
        assert_eq!(usage.cached_tokens, 42);
    }

    #[test]
    fn usage_deserializes_anthropic_cache_read() {
        let usage: Usage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"cache_read_input_tokens":37}"#,
        )
        .unwrap();
        assert_eq!(usage.cached_tokens, 37);
    }

    #[test]
    fn usage_cached_tokens_defaults_to_zero() {
        let usage: Usage =
            serde_json::from_str(r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#)
                .unwrap();
        assert_eq!(usage.cached_tokens, 0);
        let null: Usage =
            serde_json::from_str(r#"{"prompt_tokens":10,"prompt_tokens_details":null}"#).unwrap();
        assert_eq!(null.cached_tokens, 0);
    }

    #[test]
    fn usage_prefers_openai_details_over_anthropic_field() {
        let usage: Usage = serde_json::from_str(
            r#"{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":42},"cache_read_input_tokens":99}"#,
        )
        .unwrap();
        assert_eq!(usage.cached_tokens, 42);
    }
}
