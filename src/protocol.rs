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
    },
}

fn tool_result_text(c: &Value) -> String {
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
                if delta.and_then(|d| d.get("type")).and_then(Value::as_str) == Some("text_delta") {
                    if let Some(t) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                        return vec![AgentEvent::TextDelta(t.to_string())];
                    }
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

        "result" => vec![AgentEvent::TurnResult {
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            text: v.get("result").and_then(Value::as_str).map(String::from),
        }],

        _ => vec![],
    }
}
