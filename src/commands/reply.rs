//! Last user-visible assistant message from a live agent's native transcript.
use super::attach::resolve_attach_target;
use crate::agents::locate_transcript;
use crate::history::Sources;
use crate::session::session_agent;
use crate::state::read_live_session_agent_identity;
use crate::tmux::list_sessions;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ReplyResult {
    pub api_version: u32,
    pub session: String,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub available: bool,
    pub text: Option<String>,
    pub timestamp: Option<String>,
    pub channel: Option<String>,
    pub reason: Option<String>,
}

pub fn reply(target: &str) -> Result<ReplyResult> {
    let sessions = list_sessions()?;
    let session = resolve_attach_target(&sessions, target)?;
    let agent = session_agent(session);
    let mut result = ReplyResult {
        api_version: crate::API_VERSION,
        session: session.name.clone(),
        agent: agent.map(str::to_string),
        session_id: None,
        available: false,
        text: None,
        timestamp: None,
        channel: None,
        reason: None,
    };
    let Some(agent @ ("claude" | "codex" | "cortex")) = agent else {
        result.reason =
            Some("reply supports Claude Code, Codex and Cortex Code transcripts".into());
        return Ok(result);
    };
    let snapshot = crate::session::AiAttnSnapshot::load();
    let direct = crate::session::session_ai_record(session, false, false, &snapshot)
        .and_then(|record| record.session_id.clone());
    let stored = read_live_session_agent_identity(&session.name, session.created_epoch)?
        .filter(|id| id.agent == agent)
        .map(|id| id.session_id);
    // Resolve the live process first, then the identity persisted by resume or
    // respawn. A resumed transcript may predate the pane by days or months.
    result.session_id =
        super::respawn::live_session_id(&session.pane_id, agent, &Sources::from_env())
            .or(direct)
            .or(stored);
    let Some(path) = result
        .session_id
        .as_deref()
        .and_then(|id| locate_transcript(agent, id))
    else {
        result.reason = Some(
            "no transcript found for this process; check agent identity with inspect/activity"
                .into(),
        );
        return Ok(result);
    };
    let file = File::open(path).context("failed to open agent transcript")?;
    if let Some(message) = last_reply(BufReader::new(file), agent)? {
        result.available = true;
        result.text = Some(message.text);
        result.timestamp = message.timestamp;
        result.channel = message.channel;
    } else {
        result.reason = Some("no complete assistant text message in the transcript yet".into());
    }
    Ok(result)
}

#[derive(Debug)]
struct Message {
    text: String,
    timestamp: Option<String>,
    channel: Option<String>,
}

fn text_blocks(message: &Value, kind: &str) -> Option<String> {
    let text = message
        .get("content")?
        .as_array()?
        .iter()
        .filter(|block| block["type"] == kind)
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn last_reply(mut reader: impl BufRead, agent: &str) -> Result<Option<Message>> {
    let mut last = None;
    let mut fallback = None;
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        // An agent may be appending its final record; never read a partial one.
        if !bytes.ends_with(b"\n") {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let timestamp = record["timestamp"].as_str().map(str::to_string);
        if agent == "claude"
            && record["type"] == "assistant"
            && record["isSidechain"] != true
            && record["message"]["role"] == "assistant"
        {
            if let Some(text) = text_blocks(&record["message"], "text") {
                last = Some(Message {
                    text,
                    timestamp,
                    channel: None,
                });
            }
        } else if agent == "cortex" && record["role"] == "assistant" {
            if let Some(text) = text_blocks(&record, "text") {
                last = Some(Message {
                    text,
                    timestamp: record["assistant_sent_time"]
                        .as_str()
                        .map(str::to_string)
                        .or(timestamp),
                    channel: None,
                });
            }
        } else if agent == "codex" {
            let payload = &record["payload"];
            if record["type"] == "response_item"
                && payload["type"] == "message"
                && payload["role"] == "assistant"
                && payload["channel"] != "analysis"
                && let Some(text) = text_blocks(payload, "output_text")
            {
                last = Some(Message {
                    text,
                    timestamp,
                    channel: payload["channel"].as_str().map(str::to_string),
                });
            } else if record["type"] == "event_msg"
                && payload["type"] == "agent_message"
                && let Some(text) = payload["message"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
            {
                // Older rollouts may only have agent_message events. Prefer
                // response_item records when present to avoid duplicate output.
                fallback = Some(Message {
                    text: text.to_string(),
                    timestamp,
                    channel: None,
                });
            }
        }
    }
    Ok(last.or(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_reply_excludes_reasoning_tools_and_incomplete_records() {
        let records = concat!(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"channel\":\"final\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\\nline two\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"channel\":\"analysis\",\"content\":[{\"type\":\"output_text\",\"text\":\"private\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"output\":\"tool output\"}}\n",
            "malformed\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"duplicate\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"incomplete\"}]}}"
        );
        assert_eq!(
            last_reply(records.as_bytes(), "codex")
                .unwrap()
                .unwrap()
                .text,
            "answer\nline two"
        );
    }
    #[test]
    fn claude_reply_uses_text_blocks_and_ignores_sidechains_and_tools() {
        let records = concat!(
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"first\"},{\"type\":\"thinking\",\"thinking\":\"private\"},{\"type\":\"text\",\"text\":\"second\"}]}}\n",
            "{\"type\":\"assistant\",\"isSidechain\":true,\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"subagent\"}]}}\n"
        );
        assert_eq!(
            last_reply(records.as_bytes(), "claude")
                .unwrap()
                .unwrap()
                .text,
            "first\nsecond"
        );
    }

    #[test]
    fn cortex_reply_reads_direct_messages_and_skips_tools_and_partial_lines() {
        let records = concat!(
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":{\"text\":\"private\"}},{\"type\":\"text\",\"text\":\"answer\"},{\"type\":\"text\",\"text\":\"line two\"}]}\n",
            "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"question\"}]}\n",
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"tool_use\":{\"name\":\"bash\"}}]}\n",
            "malformed\n{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"partial\"}]}"
        );
        assert_eq!(
            last_reply(records.as_bytes(), "cortex")
                .unwrap()
                .unwrap()
                .text,
            "answer\nline two"
        );
    }

    #[test]
    fn cortex_reply_preserves_native_timestamp_for_freshness_checks() {
        let records = concat!(
            "{\"role\":\"assistant\",\"assistant_sent_time\":\"2026-10-30T18:00:00.000Z\",\"content\":[{\"type\":\"text\",\"text\":\"Done\"}]}\n",
            "{\"role\":\"assistant\",\"assistant_sent_time\":\"2026-10-30T18:01:00.000Z\",\"content\":[{\"type\":\"text\",\"text\":\"Done\"}]}\n"
        );
        let message = last_reply(records.as_bytes(), "cortex").unwrap().unwrap();
        assert_eq!(message.text, "Done");
        assert_eq!(
            message.timestamp.as_deref(),
            Some("2026-10-30T18:01:00.000Z")
        );
    }
}
