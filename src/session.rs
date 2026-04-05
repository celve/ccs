use chrono::{DateTime, Local, TimeZone};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::collections::HashSet;

use crate::transcript::{self, TranscriptState};

#[derive(Deserialize)]
pub struct SessionMeta {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "startedAt")]
    pub started_at: u64, // epoch milliseconds
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Idle,
    Thinking,
    Responding,
    Plan,
    Asking,
    Tool(String),
    Exited,
}

pub struct Session {
    pub cwd: String,
    pub started_at: DateTime<Local>,
    pub name: Option<String>,
    pub state: SessionState,
    pub last_activity: Option<DateTime<Local>>,
}

/// Convert a cwd path to the Claude Code project directory key.
/// Replaces every character that isn't [a-zA-Z0-9-] with '-'.
fn cwd_to_project_key(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Check if a process is alive via kill -0.
fn is_pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Find the transcript JSONL file for a session.
fn find_transcript(claude_dir: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    let projects_dir = claude_dir.join("projects");
    let key = cwd_to_project_key(cwd);
    let primary = projects_dir.join(&key).join(format!("{session_id}.jsonl"));
    if primary.exists() {
        return Some(primary);
    }

    // Fallback: search all project dirs
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join(format!("{session_id}.jsonl"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Load all sessions from ~/.claude/sessions/ and enrich with transcript data.
pub fn load_sessions() -> Vec<Session> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let claude_dir = home.join(".claude");
    let sessions_dir = claude_dir.join("sessions");

    let entries = match fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut sessions = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let meta: SessionMeta = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Convert started_at from epoch millis
        let started_at_secs = (meta.started_at / 1000) as i64;
        let started_at = match Local.timestamp_opt(started_at_secs, 0) {
            chrono::LocalResult::Single(dt) => dt,
            _ => continue,
        };

        // Check PID liveness
        let alive = is_pid_alive(meta.pid);

        // Find and read transcript
        let transcript_info =
            find_transcript(&claude_dir, &meta.cwd, &meta.session_id)
                .and_then(|path| transcript::read_transcript_tail(&path));

        let (state, last_activity) = match transcript_info {
            Some(info) => {
                let state = if !alive {
                    SessionState::Exited
                } else {
                    match info.state {
                        TranscriptState::Idle => SessionState::Idle,
                        TranscriptState::Thinking => SessionState::Thinking,
                        TranscriptState::Responding => SessionState::Responding,
                        TranscriptState::Plan => SessionState::Plan,
                        TranscriptState::Asking => SessionState::Asking,
                        TranscriptState::Tool(name) => SessionState::Tool(name),
                    }
                };
                let last_act = info.last_activity.map(|dt| dt.with_timezone(&Local));
                (state, last_act)
            }
            None => {
                let state = if alive {
                    SessionState::Idle
                } else {
                    SessionState::Exited
                };
                (state, None)
            }
        };

        sessions.push(Session {
            cwd: meta.cwd,
            started_at,
            name: meta.name,
            state,
            last_activity,
        });
    }

    // Scan for finished sessions (transcripts with no matching session file)
    load_finished_sessions(&claude_dir, &mut sessions);

    sessions
}

/// Scan transcript files for finished sessions not in the active set.
fn load_finished_sessions(claude_dir: &Path, out: &mut Vec<Session>) {
    // Collect active session IDs by re-reading session files
    let sessions_dir = claude_dir.join("sessions");
    let mut active_ids = HashSet::new();
    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<SessionMeta>(&content) {
                    active_ids.insert(meta.session_id);
                }
            }
        }
    }

    let projects_dir = claude_dir.join("projects");
    let project_dirs = match fs::read_dir(&projects_dir) {
        Ok(d) => d,
        Err(_) => return,
    };

    for proj_entry in project_dirs.flatten() {
        let proj_path = proj_entry.path();
        if !proj_path.is_dir() {
            continue;
        }

        let transcripts = match fs::read_dir(&proj_path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        for t_entry in transcripts.flatten() {
            let t_path = t_entry.path();
            if t_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let session_id = t_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");

            // Skip if this is an active session
            if active_ids.contains(session_id) {
                continue;
            }

            // Skip transcripts not modified in the last 24 hours
            if let Ok(metadata) = t_path.metadata() {
                if let Ok(modified) = metadata.modified() {
                    let age = modified.elapsed().unwrap_or_default();
                    if age.as_secs() > 86400 {
                        continue;
                    }
                }
            }

            // Extract metadata from transcript head
            let meta = match transcript::read_transcript_meta(&t_path) {
                Some(m) => m,
                None => continue,
            };

            let cwd = match meta.cwd {
                Some(c) => c,
                None => continue,
            };

            let started_at = match meta.started_at {
                Some(ts) => ts.with_timezone(&Local),
                None => continue,
            };

            // Read tail for last activity time
            let tail = transcript::read_transcript_tail(&t_path);
            let last_activity = tail
                .as_ref()
                .and_then(|info| info.last_activity)
                .map(|dt| dt.with_timezone(&Local));

            out.push(Session {
                cwd,
                started_at,
                name: None, // session file is gone, no name available
                state: SessionState::Exited,
                last_activity,
            });
        }
    }
}
