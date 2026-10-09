//! Réponse en flux (SSE, `stream: true`) : les fragments de texte sont transmis au fil de l'eau,
//! puis le flux est recomposé en un `ChatResponse` identique à celui d'une réponse non streamée.
//! Les fragments ne sont qu'un affichage : c'est le message recomposé qui fait foi.

use super::ModelError;
use super::types::{ChatResponse, ChatUsage, Message, ToolCall};
use serde_json::Value;

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

pub struct StreamAccumulator<'a> {
    on_delta: &'a mut (dyn FnMut(&str) + Send),
    /// Octets reçus mais pas encore terminés par un saut de ligne (un caractère UTF-8 peut être
    /// coupé entre deux paquets : on ne décode que des lignes complètes).
    pending: Vec<u8>,
    text: String,
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    model: Option<String>,
    usage: ChatUsage,
    done: bool,
    requested: String,
}

impl<'a> StreamAccumulator<'a> {
    pub fn new(requested: &str, on_delta: &'a mut (dyn FnMut(&str) + Send)) -> Self {
        StreamAccumulator {
            on_delta,
            pending: Vec::new(),
            text: String::new(),
            calls: Vec::new(),
            finish_reason: None,
            model: None,
            usage: ChatUsage::default(),
            done: false,
            requested: requested.to_string(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), ModelError> {
        self.pending.extend_from_slice(bytes);
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            self.line(line.trim())?;
        }
        Ok(())
    }

    fn line(&mut self, line: &str) -> Result<(), ModelError> {
        // Les lignes vides séparent les événements ; `:` introduit un commentaire (keep-alive).
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ModelError::Decode(format!("stream chunk: {e}")))?;
        if let Some(err) = v.get("error") {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error")
                .to_string();
            return Err(ModelError::Unavailable {
                attempts: 1,
                last: format!("stream interrupted: {message}"),
            });
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_string());
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null())
            && let Ok(u) = serde_json::from_value::<ChatUsage>(u.clone())
        {
            self.usage = u;
        }
        let Some(choice) = v.pointer("/choices/0") else {
            return Ok(());
        };
        if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(r.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(t) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.text.push_str(t);
            (self.on_delta)(t);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let index = c.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if index > 64 {
                    return Err(ModelError::InvalidResponse(
                        "tool call index out of range".into(),
                    ));
                }
                if self.calls.len() <= index {
                    self.calls.resize_with(index + 1, PartialCall::default);
                }
                let slot = &mut self.calls[index];
                if let Some(id) = c.get("id").and_then(Value::as_str) {
                    slot.id = id.to_string();
                }
                if let Some(n) = c.pointer("/function/name").and_then(Value::as_str) {
                    slot.name.push_str(n);
                }
                if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str) {
                    slot.arguments.push_str(a);
                }
            }
        }
        Ok(())
    }

    /// Recompose la réponse. Un flux coupé avant `[DONE]` sans `finish_reason` est une erreur :
    /// on n'agit jamais sur un message tronqué.
    pub fn finish(mut self) -> Result<ChatResponse, ModelError> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            let rest = String::from_utf8_lossy(&rest).into_owned();
            self.line(rest.trim())?;
        }
        if !self.done && self.finish_reason.is_none() {
            return Err(ModelError::Unavailable {
                attempts: 1,
                last: "stream ended before completion".into(),
            });
        }
        let mut tool_calls = Vec::new();
        for c in self.calls {
            if c.id.is_empty() || c.name.is_empty() {
                return Err(ModelError::InvalidResponse(
                    "tool call without id or name".into(),
                ));
            }
            tool_calls.push(ToolCall::new(c.id, c.name, c.arguments));
        }
        let content = Some(self.text).filter(|t| !t.is_empty());
        if content.is_none()
            && tool_calls.is_empty()
            && self.finish_reason.as_deref() != Some("stop")
        {
            return Err(ModelError::InvalidResponse(
                "empty assistant message".into(),
            ));
        }
        Ok(ChatResponse {
            message: Message::Assistant {
                content,
                tool_calls,
            },
            finish_reason: self.finish_reason,
            model: self.model.unwrap_or(self.requested),
            usage: self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Result<ChatResponse, ModelError>, Vec<String>) {
        let mut seen = Vec::new();
        let result = {
            let mut sink = |d: &str| seen.push(d.to_string());
            let mut acc = StreamAccumulator::new("req-model", &mut sink);
            let mut r = Ok(());
            for c in chunks {
                r = r.and(acc.feed(c));
            }
            r.and_then(|_| acc.finish())
        };
        (result, seen)
    }

    #[test]
    fn text_deltas_are_forwarded_and_recomposed() {
        let (r, seen) = run(&[
            b": OPENROUTER PROCESSING\n\n",
            b"data: {\"model\":\"m-1\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"cost\":0.1}}\n\n",
            b"data: [DONE]\n\n",
        ]);
        let r = r.unwrap();
        assert_eq!(seen, ["Hel", "lo"]);
        assert_eq!(r.text(), "Hello");
        assert_eq!(r.model, "m-1");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            (
                r.usage.prompt_tokens,
                r.usage.completion_tokens,
                r.usage.cost
            ),
            (5, 2, Some(0.1))
        );
    }

    #[test]
    fn packets_may_split_lines_and_utf8_characters() {
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"é\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n";
        let bytes = line.as_bytes();
        let e = line.find('é').unwrap();
        // Coupe au milieu de « é » (2 octets).
        let (r, seen) = run(&[&bytes[..e + 1], &bytes[e + 1..]]);
        assert_eq!(r.unwrap().text(), "é");
        assert_eq!(seen, ["é"]);
    }

    #[test]
    fn tool_calls_are_assembled_from_fragments() {
        let (r, seen) = run(&[
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"fs_read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n",
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            b"data: [DONE]\n",
        ]);
        let r = r.unwrap();
        assert!(seen.is_empty());
        assert_eq!(r.tool_calls().len(), 1);
        assert_eq!(r.tool_calls()[0].function.name, "fs_read");
        assert_eq!(r.tool_calls()[0].args().unwrap()["path"], "a");
        assert_eq!(r.model, "req-model");
    }

    #[test]
    fn a_truncated_stream_is_an_error_never_a_partial_message() {
        let (r, seen) = run(&[b"data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n"]);
        assert_eq!(seen, ["par"]);
        assert!(matches!(r, Err(ModelError::Unavailable { .. })), "{r:?}");
    }

    #[test]
    fn an_error_in_the_stream_is_reported() {
        let (r, _) = run(&[b"data: {\"error\":{\"message\":\"overloaded\"}}\n"]);
        assert!(r.unwrap_err().to_string().contains("overloaded"));
    }

    #[test]
    fn a_tool_call_without_a_name_is_rejected() {
        let (r, _) = run(&[
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\"}]},\"finish_reason\":\"tool_calls\"}]}\n",
            b"data: [DONE]\n",
        ]);
        assert!(matches!(r, Err(ModelError::InvalidResponse(_))));
    }
}
