use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Arguments sous forme de chaîne JSON, tels que le modèle les a produits (potentiellement invalides).
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_kind() -> String {
    "function".into()
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        ToolCall {
            id: id.into(),
            kind: function_kind(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    /// Arguments parsés ; une chaîne vide vaut `{}`. Une erreur est renvoyée au modèle, pas fatale.
    pub fn args(&self) -> Result<Value, String> {
        if self.function.arguments.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&self.function.arguments)
            .map_err(|e| format!("arguments are not valid JSON: {e}"))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(default)]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Message::System {
            content: text.into(),
        }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            content: text.into(),
        }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Message::Assistant {
            content: Some(text.into()),
            tool_calls: vec![],
        }
    }
    pub fn tool(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Message::Tool {
            tool_call_id: call_id.into(),
            content: text.into(),
        }
    }
}

/// Outil exposé au modèle : nom, description et schéma JSON des paramètres.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl Serialize for ToolSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        json!({"type": "function", "function": {"name": self.name, "description": self.description, "parameters": self.parameters}})
            .serialize(s)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// Présent via OpenRouter uniquement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatResponse {
    /// Toujours un `Message::Assistant`.
    pub message: Message,
    /// `stop`, `tool_calls`, `length`…
    pub finish_reason: Option<String>,
    /// Modèle qui a réellement répondu.
    pub model: String,
    pub usage: ChatUsage,
}

impl ChatResponse {
    pub fn text(&self) -> &str {
        match &self.message {
            Message::Assistant {
                content: Some(c), ..
            } => c,
            _ => "",
        }
    }
    pub fn tool_calls(&self) -> &[ToolCall] {
        match &self.message {
            Message::Assistant { tool_calls, .. } => tool_calls,
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_to_the_openai_shape() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                Message::system("s"),
                Message::user("u"),
                Message::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall::new("c1", "fs_read", r#"{"path":"a"}"#)],
                },
                Message::tool("c1", "contents"),
            ],
            tools: vec![ToolSpec {
                name: "fs_read".into(),
                description: "read".into(),
                parameters: json!({"type":"object"}),
            }],
            max_tokens: Some(100),
            temperature: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"][0], json!({"role": "system", "content": "s"}));
        assert_eq!(v["messages"][2]["role"], "assistant");
        assert!(
            v["messages"][2]["content"].is_null(),
            "content is present and null when only tool calls"
        );
        assert_eq!(v["messages"][2]["tool_calls"][0]["type"], "function");
        assert_eq!(
            v["messages"][2]["tool_calls"][0]["function"]["name"],
            "fs_read"
        );
        assert_eq!(
            v["messages"][3],
            json!({"role": "tool", "tool_call_id": "c1", "content": "contents"})
        );
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(
            v["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
        assert!(v.get("temperature").is_none(), "unset options are omitted");
    }

    #[test]
    fn a_request_without_tools_omits_the_field() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![Message::user("u")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        };
        assert!(serde_json::to_value(&req).unwrap().get("tools").is_none());
    }

    #[test]
    fn tool_call_arguments_are_parsed_leniently() {
        assert_eq!(ToolCall::new("1", "t", "").args().unwrap(), json!({}));
        assert_eq!(
            ToolCall::new("1", "t", r#"{"a":1}"#).args().unwrap(),
            json!({"a": 1})
        );
        assert!(
            ToolCall::new("1", "t", "{not json")
                .args()
                .unwrap_err()
                .contains("not valid JSON")
        );
    }
}
