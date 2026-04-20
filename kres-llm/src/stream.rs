//! SSE event decoding for Anthropic streaming responses.
//!
//! The Anthropic streaming format emits `event: <name>` / `data: <json>`
//! pairs. We only need a small subset here:
//! - `content_block_start` — opens a block (thinking or text).
//! - `content_block_delta` — incremental text/thinking deltas.
//! - `content_block_stop` — closes a block.
//! - `message_delta` / `message_stop` — final usage + stop reason.

use serde::Deserialize;

/// Decoded application-level event. The wrapping `event:` name from SSE
/// lives in `kind`; the `data:` payload is parsed into `payload`.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub kind: StreamEventKind,
}

#[derive(Debug, Clone)]
pub enum StreamEventKind {
    /// First event: carries the initial `usage` block (input_tokens,
    /// cache_creation_input_tokens, cache_read_input_tokens) and
    /// optionally `model` / `stop_reason` placeholders.
    MessageStart {
        input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
        model: Option<String>,
    },
    BlockStart {
        index: u32,
        block_type: String,
    },
    TextDelta {
        index: u32,
        text: String,
    },
    ThinkingDelta {
        index: u32,
        text: String,
    },
    BlockStop {
        index: u32,
    },
    /// Final delta: includes cumulative usage (output_tokens +
    /// optionally input / cache_creation / cache_read) and the
    /// terminal `stop_reason`. Each field is `Option` because
    /// Anthropic may only populate a subset on a given emission;
    /// the caller takes whichever is Some and leaves the rest
    /// undisturbed.
    MessageDelta {
        stop_reason: Option<String>,
        output_tokens: Option<u64>,
        input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
    },
    MessageStop,
    Ping,
    Other,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStartEnvelope {
    #[serde(default)]
    index: u32,
    content_block: ContentBlockStart,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStart {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDeltaEnvelope {
    #[serde(default)]
    index: u32,
    delta: ContentDelta,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStopEnvelope {
    #[serde(default)]
    index: u32,
}

#[derive(Debug, Deserialize)]
struct MessageStartEnvelope {
    message: MessageStartInner,
}

#[derive(Debug, Deserialize)]
struct MessageStartInner {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: MessageStartUsage,
}

#[derive(Debug, Default, Deserialize)]
struct MessageStartUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaEnvelope {
    delta: MessageDeltaInner,
    #[serde(default)]
    usage: MessageDeltaUsage,
}

/// `message_delta` carries the CUMULATIVE usage for the whole call.
/// For streaming responses Anthropic typically populates the cache
/// stats here rather than in `message_start` (message_start holds
/// them too but sometimes only the input side). Pulling them from
/// both events gives us whichever value lands first. Observed in
/// session 870217e4: cache_read_input_tokens was absent from
/// message_start but present in message_delta.
#[derive(Debug, Default, Deserialize)]
struct MessageDeltaUsage {
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaInner {
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Parse one SSE `event:` name plus `data:` payload into a StreamEvent.
/// Unrecognised events become `StreamEventKind::Other` so the caller
/// can log-and-skip.
pub fn parse_event(event_name: &str, data: &str) -> Result<StreamEvent, serde_json::Error> {
    let kind = match event_name {
        "message_start" => {
            let env: MessageStartEnvelope = serde_json::from_str(data)?;
            StreamEventKind::MessageStart {
                input_tokens: env.message.usage.input_tokens,
                cache_creation_input_tokens: env.message.usage.cache_creation_input_tokens,
                cache_read_input_tokens: env.message.usage.cache_read_input_tokens,
                model: env.message.model,
            }
        }
        "content_block_start" => {
            let env: ContentBlockStartEnvelope = serde_json::from_str(data)?;
            StreamEventKind::BlockStart {
                index: env.index,
                block_type: env.content_block.kind,
            }
        }
        "content_block_delta" => {
            let env: ContentBlockDeltaEnvelope = serde_json::from_str(data)?;
            match env.delta {
                ContentDelta::TextDelta { text } => StreamEventKind::TextDelta {
                    index: env.index,
                    text,
                },
                ContentDelta::ThinkingDelta { thinking } => StreamEventKind::ThinkingDelta {
                    index: env.index,
                    text: thinking,
                },
                ContentDelta::Other => StreamEventKind::Other,
            }
        }
        "content_block_stop" => {
            let env: ContentBlockStopEnvelope = serde_json::from_str(data)?;
            StreamEventKind::BlockStop { index: env.index }
        }
        "message_delta" => {
            let env: MessageDeltaEnvelope = serde_json::from_str(data)?;
            StreamEventKind::MessageDelta {
                stop_reason: env.delta.stop_reason,
                output_tokens: env.usage.output_tokens,
                input_tokens: env.usage.input_tokens,
                cache_creation_input_tokens: env.usage.cache_creation_input_tokens,
                cache_read_input_tokens: env.usage.cache_read_input_tokens,
            }
        }
        "message_stop" => StreamEventKind::MessageStop,
        "ping" => StreamEventKind::Ping,
        _ => StreamEventKind::Other,
    };
    Ok(StreamEvent { kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let ev = parse_event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        )
        .unwrap();
        match ev.kind {
            StreamEventKind::TextDelta { index, text } => {
                assert_eq!(index, 0);
                assert_eq!(text, "Hello");
            }
            _ => panic!("wrong kind: {:?}", ev.kind),
        }
    }

    #[test]
    fn parses_thinking_delta() {
        let ev = parse_event(
            "content_block_delta",
            r#"{"index":1,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        )
        .unwrap();
        match ev.kind {
            StreamEventKind::ThinkingDelta { index, text } => {
                assert_eq!(index, 1);
                assert_eq!(text, "hmm");
            }
            _ => panic!("wrong kind: {:?}", ev.kind),
        }
    }

    #[test]
    fn parses_block_start_thinking() {
        let ev = parse_event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        )
        .unwrap();
        match ev.kind {
            StreamEventKind::BlockStart { index, block_type } => {
                assert_eq!(index, 0);
                assert_eq!(block_type, "thinking");
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn parses_block_stop() {
        let ev = parse_event("content_block_stop", r#"{"index":2}"#).unwrap();
        match ev.kind {
            StreamEventKind::BlockStop { index } => assert_eq!(index, 2),
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn parses_message_delta_with_stop_reason() {
        let ev = parse_event(
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}"#,
        )
        .unwrap();
        match ev.kind {
            StreamEventKind::MessageDelta {
                stop_reason,
                output_tokens,
                ..
            } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(output_tokens, Some(10));
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn parses_message_delta_with_cache_read() {
        // Session 870217e4 symptom: cache_read_input_tokens absent
        // from message_start, present on message_delta. Make sure
        // the parser picks it up from the delta.
        let ev = parse_event(
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42,"input_tokens":5,"cache_creation_input_tokens":100,"cache_read_input_tokens":7050}}"#,
        )
        .unwrap();
        match ev.kind {
            StreamEventKind::MessageDelta {
                output_tokens,
                input_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                assert_eq!(output_tokens, Some(42));
                assert_eq!(input_tokens, Some(5));
                assert_eq!(cache_creation_input_tokens, Some(100));
                assert_eq!(cache_read_input_tokens, Some(7050));
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn parses_message_start_usage() {
        let ev = parse_event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","model":"claude-opus-4-7","usage":{"input_tokens":100,"cache_creation_input_tokens":50,"cache_read_input_tokens":20}}}"#,
        ).unwrap();
        match ev.kind {
            StreamEventKind::MessageStart {
                input_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                model,
            } => {
                assert_eq!(input_tokens, 100);
                assert_eq!(cache_creation_input_tokens, 50);
                assert_eq!(cache_read_input_tokens, 20);
                assert_eq!(model.as_deref(), Some("claude-opus-4-7"));
            }
            _ => panic!("wrong kind: {:?}", ev.kind),
        }
    }

    #[test]
    fn parses_message_stop() {
        let ev = parse_event("message_stop", "{}").unwrap();
        matches!(ev.kind, StreamEventKind::MessageStop);
    }

    #[test]
    fn ping_is_recognized() {
        let ev = parse_event("ping", "{}").unwrap();
        matches!(ev.kind, StreamEventKind::Ping);
    }

    #[test]
    fn unknown_event_is_other_not_error() {
        let ev = parse_event("mystery", r#"{"whatever":true}"#).unwrap();
        matches!(ev.kind, StreamEventKind::Other);
    }

    #[test]
    fn unknown_delta_type_is_other() {
        let ev = parse_event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"future_delta","value":1}}"#,
        )
        .unwrap();
        matches!(ev.kind, StreamEventKind::Other);
    }

    #[test]
    fn malformed_payload_returns_error() {
        let e = parse_event("content_block_delta", "not json");
        assert!(e.is_err());
    }
}
