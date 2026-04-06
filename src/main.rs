mod session;
mod transcript;

use chrono::Local;
use clap::Parser;
use colored::Colorize;
use session::{Session, SessionState};

#[derive(Parser)]
#[command(name = "ccs", about = "Claude Code Sessions — view active session status")]
struct Cli {
    /// Show last N sessions (default: 10)
    #[arg(short = 'n', long = "count", default_value_t = 10)]
    count: usize,

    /// Filter by time range (e.g., 30m, 2h, 7d)
    #[arg(short = 't', long = "time")]
    time: Option<String>,

    /// Filter by path substring (case-insensitive)
    #[arg(short = 'p', long = "path")]
    path: Option<String>,

    /// Filter by state (working, idle, exited)
    #[arg(short = 's', long = "state")]
    state: Option<String>,

    /// Output as JSON
    #[arg(short = 'j', long = "json")]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    let now = Local::now();

    let mut sessions = session::load_sessions();

    // Apply filters
    if let Some(ref pattern) = cli.path {
        let pat = pattern.to_lowercase();
        sessions.retain(|s| s.cwd.to_lowercase().contains(&pat));
    }

    if let Some(ref state_filter) = cli.state {
        let filter = state_filter.to_lowercase();
        sessions.retain(|s| match &s.state {
            SessionState::Idle => filter == "idle",
            SessionState::Thinking | SessionState::Responding | SessionState::Tool(_) => {
                filter == "working" || filter == "thinking" || filter == "responding"
                    || filter == "tool"
                    || state_label_plain(&s.state).to_lowercase().contains(&filter)
            }
            SessionState::Plan => filter == "planned" || filter == "plan" || filter == "waiting",
            SessionState::Asking => filter == "asking" || filter == "waiting",
            SessionState::Exited => filter == "exited",
        });
    }

    if let Some(ref time_str) = cli.time {
        match parse_duration(time_str) {
            Some(dur) => {
                sessions.retain(|s| {
                    let activity = s.last_activity.unwrap_or(s.started_at);
                    now.signed_duration_since(activity).num_seconds() <= dur.num_seconds()
                });
            }
            None => {
                eprintln!("Invalid duration: {time_str} (expected: 30m, 2h, 7d, etc.)");
                std::process::exit(1);
            }
        }
    }

    // Sort by last activity descending (most recent first)
    sessions.sort_by(|a, b| {
        let a_time = a.last_activity.unwrap_or(a.started_at);
        let b_time = b.last_activity.unwrap_or(b.started_at);
        b_time.cmp(&a_time)
    });

    // Apply count limit
    sessions.truncate(cli.count);

    if sessions.is_empty() {
        if cli.json {
            println!("[]");
        } else {
            println!("No sessions found.");
        }
        return;
    }

    if cli.json {
        render_json(&sessions, &now);
    } else {
        let tree = build_tree(&sessions);
        render_tree(&tree, &now);
    }
}

// --- Duration parsing ---

fn parse_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: i64 = num_str.parse().ok()?;
    match unit {
        "s" => Some(chrono::Duration::seconds(num)),
        "m" => Some(chrono::Duration::minutes(num)),
        "h" => Some(chrono::Duration::hours(num)),
        "d" => Some(chrono::Duration::days(num)),
        _ => None,
    }
}

// --- Tree construction ---

struct TreeNode<'a> {
    session: &'a Session,
    children: Vec<TreeNode<'a>>,
}

fn build_tree(sessions: &[Session]) -> Vec<TreeNode<'_>> {
    let mut roots: Vec<TreeNode<'_>> = Vec::new();

    // Sort by cwd length (shortest first) so parents are processed before children
    let mut by_cwd_len: Vec<&Session> = sessions.iter().collect();
    by_cwd_len.sort_by_key(|s| s.cwd.len());

    for sess in by_cwd_len {
        let mut inserted = false;
        for root in &mut roots {
            if try_insert(root, sess) {
                inserted = true;
                break;
            }
        }
        if !inserted {
            roots.push(TreeNode {
                session: sess,
                children: Vec::new(),
            });
        }
    }

    // Restore sort order: most recent activity first
    sort_tree_nodes(&mut roots);
    roots
}

/// Try to insert session as a descendant of node. Returns true if inserted.
fn try_insert<'a>(node: &mut TreeNode<'a>, session: &'a Session) -> bool {
    let parent_cwd = &node.session.cwd;
    // Check strict prefix with '/' boundary
    if session.cwd.starts_with(parent_cwd)
        && session.cwd.len() > parent_cwd.len()
        && session.cwd.as_bytes()[parent_cwd.len()] == b'/'
    {
        // Try deeper children first (deepest match wins)
        for child in &mut node.children {
            if try_insert(child, session) {
                return true;
            }
        }
        // Insert as direct child
        node.children.push(TreeNode {
            session,
            children: Vec::new(),
        });
        return true;
    }
    false
}

fn sort_tree_nodes(nodes: &mut [TreeNode<'_>]) {
    nodes.sort_by(|a, b| {
        let a_time = a.session.last_activity.unwrap_or(a.session.started_at);
        let b_time = b.session.last_activity.unwrap_or(b.session.started_at);
        b_time.cmp(&a_time)
    });
    for node in nodes.iter_mut() {
        sort_tree_nodes(&mut node.children);
    }
}

// --- Rendering ---

/// A pre-computed row with both plain-text widths and colored display strings.
struct Row {
    prefix: String,       // tree prefix (connectors + indentation)
    state_plain: String,  // "WORKING" / "IDLE" / "EXITED"
    state_colored: String,
    cwd: String,          // plain text
    name: String,         // plain text (including brackets) or empty
    name_colored: String,
    activity: String,     // plain text
    started: String,      // plain text
}

fn render_tree(nodes: &[TreeNode<'_>], now: &chrono::DateTime<Local>) {
    let home = dirs::home_dir().unwrap_or_default();
    let home_str = home.to_string_lossy();

    // Phase 1: collect all rows
    let mut rows = Vec::new();
    for node in nodes {
        collect_rows(node, now, &home_str, "", None, None, &mut rows);
    }

    if rows.is_empty() {
        return;
    }

    // Phase 2: compute column widths
    let max_prefix_state = rows
        .iter()
        .map(|r| r.prefix.chars().count() + r.state_plain.len())
        .max()
        .unwrap_or(0);
    let max_cwd = rows.iter().map(|r| r.cwd.len()).max().unwrap_or(0);
    let max_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    let max_activity = rows.iter().map(|r| r.activity.len()).max().unwrap_or(0);

    // Phase 3: print aligned
    for row in &rows {
        let prefix_state_width = row.prefix.chars().count() + row.state_plain.len();
        let ps_pad = max_prefix_state - prefix_state_width;
        let cwd_pad = max_cwd - row.cwd.len();
        let name_pad = max_name - row.name.len();

        println!(
            "{}{}{:ps_pad$}  {}{:cwd_pad$}  {}{:name_pad$}  {:>act_w$}  {}",
            row.prefix,
            row.state_colored,
            "",
            row.cwd,
            "",
            row.name_colored,
            "",
            row.activity,
            row.started,
            ps_pad = ps_pad,
            cwd_pad = cwd_pad,
            name_pad = name_pad,
            act_w = max_activity,
        );
    }
}

fn collect_rows(
    node: &TreeNode<'_>,
    now: &chrono::DateTime<Local>,
    home_str: &str,
    prefix: &str,
    connector: Option<&str>,
    parent_cwd: Option<&str>,
    rows: &mut Vec<Row>,
) {
    let sess = node.session;

    let plain = state_label_plain(&sess.state);
    let state_colored = state_label_colored(&sess.state);
    let state_plain = plain;

    let display_cwd = match parent_cwd {
        Some(pcwd) if sess.cwd.len() > pcwd.len() + 1 => {
            sess.cwd[pcwd.len() + 1..].to_string()
        }
        _ => shorten_home(&sess.cwd, home_str),
    };

    let (name_plain, name_colored) = match &sess.name {
        Some(n) => {
            let truncated = if n.len() > 40 {
                format!("{}...", &n[..37])
            } else {
                n.clone()
            };
            let plain = format!("  [{}]", truncated);
            let colored = format!("  [{}]", truncated.dimmed());
            (plain, colored)
        }
        None => (String::new(), String::new()),
    };

    let activity_time = sess.last_activity.unwrap_or(sess.started_at);
    let activity = format_relative(now.signed_duration_since(activity_time));

    let started = sess.started_at.format("%H:%M").to_string();

    let full_prefix = format!("{}{}", prefix, connector.unwrap_or(""));

    rows.push(Row {
        prefix: full_prefix,
        state_plain,
        state_colored,
        cwd: display_cwd,
        name: name_plain,
        name_colored,
        activity,
        started,
    });

    // Recurse into children
    let child_count = node.children.len();
    for (ci, child) in node.children.iter().enumerate() {
        let is_last = ci == child_count - 1;
        let child_connector = if is_last {
            "\u{2514}\u{2500}\u{2500} "
        } else {
            "\u{251c}\u{2500}\u{2500} "
        };
        let child_prefix = if connector.is_some() {
            if is_last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}\u{2502}   ")
            }
        } else {
            String::new()
        };
        collect_rows(
            child,
            now,
            home_str,
            &child_prefix,
            Some(child_connector),
            Some(&sess.cwd),
            rows,
        );
    }
}

fn shorten_home(path: &str, home: &str) -> String {
    if let Some(rest) = path.strip_prefix(home) {
        format!("~{rest}")
    } else {
        path.to_string()
    }
}

fn state_label_plain(state: &SessionState) -> String {
    match state {
        SessionState::Idle => "IDLE".into(),
        SessionState::Thinking => "THINKING".into(),
        SessionState::Responding => "RESPONDING".into(),
        SessionState::Plan => "PLANNED".into(),
        SessionState::Asking => "ASKING".into(),
        SessionState::Tool(name) => format!("TOOL:{name}"),
        SessionState::Exited => "EXITED".into(),
    }
}

fn state_label_colored(state: &SessionState) -> String {
    match state {
        SessionState::Idle => "IDLE".green().to_string(),
        SessionState::Thinking => "THINKING".yellow().bold().to_string(),
        SessionState::Responding => "RESPONDING".yellow().bold().to_string(),
        SessionState::Plan => "PLANNED".cyan().to_string(),
        SessionState::Asking => "ASKING".cyan().to_string(),
        SessionState::Tool(name) => format!("TOOL:{name}").blue().to_string(),
        SessionState::Exited => "EXITED".dimmed().to_string(),
    }
}

fn render_json(sessions: &[Session], now: &chrono::DateTime<Local>) {
    let home = dirs::home_dir().unwrap_or_default();
    let home_str = home.to_string_lossy();

    let entries: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let activity_time = s.last_activity.unwrap_or(s.started_at);
            serde_json::json!({
                "state": state_label_plain(&s.state),
                "cwd": s.cwd,
                "cwd_short": shorten_home(&s.cwd, &home_str),
                "name": s.name,
                "last_activity": activity_time.to_rfc3339(),
                "last_activity_relative": format_relative(now.signed_duration_since(activity_time)),
                "started_at": s.started_at.to_rfc3339(),
                "started_at_time": s.started_at.format("%H:%M").to_string(),
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&entries).unwrap());
}

fn format_relative(dur: chrono::Duration) -> String {
    let secs = dur.num_seconds();
    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        let rem_mins = mins % 60;
        if rem_mins > 0 {
            return format!("{hours}h {rem_mins}m ago");
        }
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours > 0 {
        format!("{days}d {rem_hours}h ago")
    } else {
        format!("{days}d ago")
    }
}
