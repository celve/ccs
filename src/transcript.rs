use chrono::{DateTime, FixedOffset};
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptState {
    Idle,
    Thinking,
    Responding,
    Plan,
    Asking,
    Tool(String), // tool name
}

pub struct TranscriptInfo {
    pub state: TranscriptState,
    pub last_activity: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    subtype: Option<String>,
    timestamp: Option<String>,
    message: Option<MessagePayload>,
}

#[derive(Deserialize)]
struct MessagePayload {
    role: Option<String>,
    stop_reason: Option<serde_json::Value>,
    content: Option<serde_json::Value>,
}

/// Read the tail of a transcript JSONL and determine session state + last activity.
pub fn read_transcript_tail(path: &Path) -> Option<TranscriptInfo> {
    let mut file = File::open(path).ok()?;
    let file_size = file.metadata().ok()?.len();

    // Read last 64KB (or whole file if smaller)
    let read_from = file_size.saturating_sub(65536);
    file.seek(SeekFrom::Start(read_from)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;

    let lines: Vec<&str> = buf.lines().collect();
    // If we seeked into the middle, discard the first (likely partial) line
    let start = if read_from > 0 { 1 } else { 0 };
    let lines = &lines[start..];

    // Parse all lines, collecting successfully parsed entries
    let entries: Vec<Entry> = lines
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // Find last activity timestamp (scan backward)
    let last_activity = entries
        .iter()
        .rev()
        .find_map(|e| {
            e.timestamp
                .as_ref()
                .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        });

    // Determine state by scanning backward for the first state-relevant entry
    let state = determine_state(&entries, &last_activity);

    Some(TranscriptInfo {
        state,
        last_activity,
    })
}

fn determine_state(
    entries: &[Entry],
    last_activity: &Option<DateTime<FixedOffset>>,
) -> TranscriptState {
    // If the last activity was more than 30 seconds ago, any "streaming" state
    // (stop_reason=None) is stale — the session is actually idle.
    let stale = last_activity
        .map(|ts| {
            let elapsed = chrono::Utc::now().signed_duration_since(ts);
            elapsed.num_seconds() > 600
        })
        .unwrap_or(false);
    for entry in entries.iter().rev() {
        let entry_type = entry.entry_type.as_deref().unwrap_or("");

        // system turn_duration → IDLE
        if entry_type == "system" && entry.subtype.as_deref() == Some("turn_duration") {
            return TranscriptState::Idle;
        }

        // Skip non-state-relevant entries
        if matches!(
            entry_type,
            "file-history-snapshot" | "attachment" | "permission-mode" | "progress" | "last-prompt"
        ) {
            continue;
        }

        if let Some(msg) = &entry.message {
            let role = msg.role.as_deref().unwrap_or("");
            let stop_reason = &msg.stop_reason;

            match role {
                "assistant" => match stop_reason {
                    Some(serde_json::Value::String(s)) if s == "end_turn" => {
                        return TranscriptState::Idle;
                    }
                    Some(serde_json::Value::String(s)) if s == "tool_use" => {
                        return classify_tool_use(&msg.content);
                    }
                    // null or None → still streaming; check content type
                    // But if the timestamp is stale (>30s ago), treat as idle
                    Some(serde_json::Value::Null) | None => {
                        if stale {
                            return TranscriptState::Idle;
                        }
                        return classify_streaming(&msg.content);
                    }
                    // Any other string (e.g., "max_tokens")
                    _ => return TranscriptState::Idle,
                },
                "user" => {
                    if let Some(content) = &msg.content {
                        match content {
                            // Real user prompt → AI is about to start
                            serde_json::Value::String(_) => return TranscriptState::Thinking,
                            // Tool result or other array content → AI continuing
                            serde_json::Value::Array(_) => return TranscriptState::Thinking,
                            _ => continue,
                        }
                    }
                }
                _ => continue,
            }
        }
    }

    TranscriptState::Idle
}

/// Classify a tool_use stop: extract tool name and determine state.
fn classify_tool_use(content: &Option<serde_json::Value>) -> TranscriptState {
    let Some(serde_json::Value::Array(arr)) = content else {
        return TranscriptState::Tool("?".into());
    };

    // Find the last tool_use entry in the content array
    let tool_name = arr
        .iter()
        .rev()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .and_then(|item| item.get("name").and_then(|n| n.as_str()));

    match tool_name {
        Some("ExitPlanMode") => TranscriptState::Plan,
        Some("AskUserQuestion") => TranscriptState::Asking,
        Some(name) => TranscriptState::Tool(name.into()),
        None => TranscriptState::Tool("?".into()),
    }
}

/// Classify a streaming assistant message by its content type.
fn classify_streaming(content: &Option<serde_json::Value>) -> TranscriptState {
    let Some(serde_json::Value::Array(arr)) = content else {
        return TranscriptState::Thinking;
    };

    // Check the last content block type
    let last_type = arr
        .iter()
        .rev()
        .find_map(|item| item.get("type").and_then(|t| t.as_str()));

    match last_type {
        Some("text") => TranscriptState::Responding,
        _ => TranscriptState::Thinking, // "thinking" or unknown
    }
}
