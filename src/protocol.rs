//! Parsing of the `claude` CLI headless `stream-json` (NDJSON) event stream.
//!
//! One line can yield several events (an assistant message may carry text plus
//! multiple tool_use blocks), so `parse_line` returns a `Vec`. Unknown shapes
//! yield an empty vec rather than crashing.

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Init {
        session_id: Option<String>,
        model: Option<String>,
        slash_commands: Vec<String>,
    },
    TextDelta(String),
    /// Streaming extended-thinking text (shown dim while the agent reasons).
    ThinkingDelta(String),
    AssistantFinal(String),
    /// A tool the agent invoked (Edit/Write/Bash/Read/…), with its input.
    ToolCall {
        name: String,
        input: Value,
    },
    /// The result of a tool call (stdout, file content, error).
    ToolResult {
        ok: bool,
        text: String,
    },
    TurnResult {
        cost_usd: f64,
        is_error: bool,
        text: Option<String>,
        /// Input-side tokens of the final request (incl. cache) ≈ context size.
        context_tokens: u64,
    },
}

pub fn tool_result_text(c: &Value) -> String {
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        let mut s = String::new();
        for b in arr {
            if let Some(t) = b.get("text").and_then(Value::as_str) {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(t);
            }
        }
        return s;
    }
    String::new()
}

pub fn parse_line(line: &str) -> Vec<AgentEvent> {
    let line = line.trim();
    if line.is_empty() {
        return vec![];
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "system" => {
            if v.get("subtype").and_then(Value::as_str) == Some("init") {
                let slash_commands = v
                    .get("slash_commands")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(|s| s.trim_start_matches('/').to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                vec![AgentEvent::Init {
                    session_id: v.get("session_id").and_then(Value::as_str).map(String::from),
                    model: v.get("model").and_then(Value::as_str).map(String::from),
                    slash_commands,
                }]
            } else {
                vec![]
            }
        }

        // Token-level streaming (present with --include-partial-messages).
        "stream_event" => {
            let ev = v.get("event");
            let etype = ev
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if etype == "content_block_delta" {
                let delta = ev.and_then(|e| e.get("delta"));
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                            return vec![AgentEvent::TextDelta(t.to_string())];
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) =
                            delta.and_then(|d| d.get("thinking")).and_then(Value::as_str)
                        {
                            return vec![AgentEvent::ThinkingDelta(t.to_string())];
                        }
                    }
                    _ => {}
                }
            }
            vec![]
        }

        // A full assistant message: text blocks and/or tool_use blocks.
        "assistant" => {
            let mut text = String::new();
            let mut tools: Vec<(String, Value)> = Vec::new();
            if let Some(arr) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in arr {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            tools.push((name, input));
                        }
                        _ => {}
                    }
                }
            }
            let mut out = Vec::new();
            if tools.is_empty() {
                if !text.trim().is_empty() {
                    out.push(AgentEvent::AssistantFinal(text));
                }
            } else {
                // text (if any) already arrived via deltas; emit the tool calls
                for (name, input) in tools {
                    out.push(AgentEvent::ToolCall { name, input });
                }
            }
            out
        }

        // Tool results come back as a "user" message with tool_result blocks.
        "user" => {
            let mut out = Vec::new();
            if let Some(arr) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in arr {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let ok = !block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let text = block
                            .get("content")
                            .map(tool_result_text)
                            .unwrap_or_default();
                        out.push(AgentEvent::ToolResult { ok, text });
                    }
                }
            }
            out
        }

        "result" => {
            let usage = v.get("usage");
            let tok = |k: &str| {
                usage
                    .and_then(|u| u.get(k))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            let context_tokens = tok("input_tokens")
                + tok("cache_read_input_tokens")
                + tok("cache_creation_input_tokens");
            vec![AgentEvent::TurnResult {
                cost_usd: v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
                is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                text: v.get("result").and_then(Value::as_str).map(String::from),
                context_tokens,
            }]
        }

        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_line() {
        let l = r#"{"type":"system","subtype":"init","session_id":"abc","model":"claude-x","slash_commands":["/foo","/bar"]}"#;
        match &parse_line(l)[..] {
            [AgentEvent::Init { session_id, model, slash_commands }] => {
                assert_eq!(session_id.as_deref(), Some("abc"));
                assert_eq!(model.as_deref(), Some("claude-x"));
                assert_eq!(slash_commands, &["foo", "bar"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn text_and_thinking_deltas() {
        let t = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}}"#;
        assert!(matches!(&parse_line(t)[..], [AgentEvent::TextDelta(s)] if s == "hi"));
        let th = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#;
        assert!(matches!(&parse_line(th)[..], [AgentEvent::ThinkingDelta(s)] if s == "hmm"));
    }

    #[test]
    fn assistant_tools_and_result_usage() {
        let a = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        match &parse_line(a)[..] {
            [AgentEvent::ToolCall { name, input }] => {
                assert_eq!(name, "Bash");
                assert_eq!(input.get("command").and_then(Value::as_str), Some("ls"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        let r = r#"{"type":"result","total_cost_usd":0.5,"is_error":false,"result":"done","usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5}}"#;
        match &parse_line(r)[..] {
            [AgentEvent::TurnResult { cost_usd, is_error, text, context_tokens }] => {
                assert_eq!(*cost_usd, 0.5);
                assert!(!is_error);
                assert_eq!(text.as_deref(), Some("done"));
                assert_eq!(*context_tokens, 105);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn garbage_is_ignored() {
        assert!(parse_line("not json").is_empty());
        assert!(parse_line("").is_empty());
        assert!(parse_line(r#"{"type":"weird"}"#).is_empty());
    }
}
