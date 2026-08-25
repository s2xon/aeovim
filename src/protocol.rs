//! Parsing of the `claude` CLI headless `stream-json` (NDJSON) event stream.
//!
//! One line can yield several events (an assistant message may carry text plus
//! multiple tool_use blocks), so `parse_line` returns a `Vec`. Unknown shapes
//! yield an empty vec rather than crashing.
//!
//! Shapes verified live against claude 2.1.241 (see scratchpad probe):
//! - `assistant` messages carry `tool_use` blocks with an `id`; the matching
//!   result arrives as a `user` message `tool_result` block with `tool_use_id`.
//! - `control_response {subtype:"success", request_id}` acks our
//!   `control_request {subtype:"interrupt"}`; the interrupted turn then ends
//!   with `result {subtype:"error_during_execution", is_error:true}` and the
//!   child stays alive for the next turn.

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Init {
        session_id: Option<String>,
        model: Option<String>,
        slash_commands: Vec<String>,
    },
    TextDelta(String),
    /// Thinking is streaming — used only as an activity pulse, content unshown.
    ThinkingDelta,
    /// The authoritative text of one assistant message. Replaces whatever the
    /// deltas accumulated for that message (deltas can be lossy; this never is).
    AssistantText(String),
    /// A tool the agent invoked (Edit/Write/Bash/Read/…), with its input.
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of a tool call, correlated back by `tool_use_id`.
    ToolResult {
        id: String,
        ok: bool,
        text: String,
    },
    TurnResult {
        cost_usd: f64,
        is_error: bool,
        subtype: String,
    },
    /// Ack for a control_request we sent (e.g. interrupt).
    ControlDone { ok: bool },
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
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                            return vec![AgentEvent::TextDelta(t.to_string())];
                        }
                    }
                    Some("thinking_delta") => return vec![AgentEvent::ThinkingDelta],
                    _ => {}
                }
            }
            vec![]
        }

        // A full assistant message: text blocks and/or tool_use blocks. The text
        // is authoritative — it supersedes whatever the deltas accumulated.
        "assistant" => {
            let mut text = String::new();
            let mut out = Vec::new();
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
                            let id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            out.push(AgentEvent::ToolCall { id, name, input });
                        }
                        _ => {}
                    }
                }
            }
            if !text.trim().is_empty() {
                // Text first, then the tool calls that followed it.
                out.insert(0, AgentEvent::AssistantText(text));
            }
            out
        }

        // Tool results come back as a "user" message with tool_result blocks.
        // (Plain user text blocks are our own turns echoed back — ignored.)
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
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let text = block
                            .get("content")
                            .map(tool_result_text)
                            .unwrap_or_default();
                        out.push(AgentEvent::ToolResult { id, ok, text });
                    }
                }
            }
            out
        }

        // result.text is not surfaced — every assistant message already arrived
        // as an `assistant` event; re-adding it duplicated the final message.
        "result" => vec![AgentEvent::TurnResult {
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            subtype: v
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }],

        "control_response" => {
            let resp = v.get("response").unwrap_or(&Value::Null);
            let ok = resp.get("subtype").and_then(Value::as_str) == Some("success")
                || v.get("subtype").and_then(Value::as_str) == Some("success");
            vec![AgentEvent::ControlDone { ok }]
        }

        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_with_text_and_tools_keeps_both() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"I'll read the file."},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/x"}}
        ]}}"#
        .replace('\n', "");
        let evs = parse_line(&line);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], AgentEvent::AssistantText(t) if t == "I'll read the file."));
        assert!(matches!(&evs[1], AgentEvent::ToolCall { id, name, .. }
            if id == "toolu_1" && name == "Read"));
    }

    #[test]
    fn tool_result_carries_id() {
        let line = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":"hello"}
        ]}}"#
        .replace('\n', "");
        let evs = parse_line(&line);
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], AgentEvent::ToolResult { id, ok: true, text }
            if id == "toolu_1" && text == "hello"));
    }

    #[test]
    fn result_subtype_surfaces() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"total_cost_usd":0.01}"#;
        let evs = parse_line(line);
        assert!(matches!(&evs[0], AgentEvent::TurnResult { is_error: true, subtype, .. }
            if subtype == "error_during_execution"));
    }

    #[test]
    fn control_response_parses() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#;
        let evs = parse_line(line);
        assert!(matches!(&evs[0], AgentEvent::ControlDone { ok: true }));
    }

    #[test]
    fn garbage_lines_are_silent() {
        assert!(parse_line("not json").is_empty());
        assert!(parse_line("").is_empty());
        assert!(parse_line(r#"{"type":"wat"}"#).is_empty());
    }
}
