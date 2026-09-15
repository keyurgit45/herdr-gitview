//! The herdr *agent* surface (as opposed to the raw *pane* surface that
//! `deliver_notes` uses today).
//!
//! Two things make this more than a thin CLI wrapper:
//!
//! 1. `agent prompt --wait` does NOT track turns, and `agent wait` is
//!    level-triggered — `agent wait <p> --until working` matches an agent
//!    that is *already* working, instantly. So prompting a busy agent and
//!    waiting returns when the PREVIOUS turn finishes, and you attribute
//!    that turn's text to your thread. The only defence is a pre-send
//!    resting gate plus a `state_change_seq` delta check, both here.
//!
//! 2. Claude Code is an alternate-screen TUI with zero host scrollback
//!    (`max_offset_from_bottom: 0`), so any reply taller than the viewport
//!    is unrecoverable by screen reading — and nothing in the API says so.
//!    `truncated: false` only means the `--lines` cap was not hit. The
//!    agent's own transcript JSONL is the only lossless channel; the screen
//!    is a fallback that must be allowed to visibly degrade, never to lie.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
}

impl AgentStatus {
    /// The four literal herdr spellings. Anything else is `Unknown`, which
    /// is deliberately NON-resting: a status herdr adds later must never
    /// accidentally read as "safe to prompt".
    pub fn from_wire(s: &str) -> AgentStatus {
        match s {
            "idle" => AgentStatus::Idle,
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::Blocked,
            "done" => AgentStatus::Done,
            _ => AgentStatus::Unknown,
        }
    }

    /// Only `Idle` and `Done` are safe to prompt into.
    pub fn is_resting(self) -> bool {
        matches!(self, AgentStatus::Idle | AgentStatus::Done)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub agent: String,
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub pane: String,
    pub agent: String,
    pub status: AgentStatus,
    pub state_change_seq: u64,
    pub session: Option<SessionRef>,
}

/// Everything needed to attribute a reply back to the turn that caused it.
#[derive(Debug, Clone)]
pub struct Handle {
    pub pane: String,
    pub seq0: u64,
    pub transcript: Option<PathBuf>,
    /// Byte length of the transcript before sending — the watermark.
    pub offset: u64,
    pub prompt: String,
}

#[derive(Debug)]
pub enum PromptError {
    /// Rejected before any input was sent — the prompt was NOT delivered.
    Blocked,
    /// The agent was mid-turn. We refuse rather than mis-attribute.
    Busy(AgentStatus),
    /// Accepted, but herdr observed no working/blocked within its 5000ms gate.
    Stalled,
    /// Our `--timeout` expired. Delivery is UNPROVEN, not disproven.
    Timeout,
    Cli(String),
}

impl PromptError {
    /// `Some(false)` = definitely not delivered, safe to offer a resend.
    /// `None` = unknown; never auto-resend.
    pub fn delivered(&self) -> Option<bool> {
        match self {
            PromptError::Blocked | PromptError::Busy(_) => Some(false),
            PromptError::Stalled | PromptError::Timeout => None,
            PromptError::Cli(_) => None,
        }
    }
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::Blocked => write!(f, "agent is blocked (waiting on a prompt)"),
            PromptError::Busy(s) => write!(f, "agent is {} — queued until it rests", s.as_str()),
            PromptError::Stalled => write!(f, "agent never started working"),
            PromptError::Timeout => write!(f, "timed out; delivery unknown"),
            PromptError::Cli(e) => write!(f, "{e}"),
        }
    }
}

/// Run the herdr CLI, returning stdout on success and the parsed error code
/// (or raw stderr) on failure. `herdr_cli::run_json` discards stderr, which
/// turns `agent_not_idle` into a silent `None`; this must not.
pub fn run_text(bin: &OsStr, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // herdr errors are JSON on stderr: {"error":{"code":..,"message":..}}
    if let Ok(v) = serde_json::from_str::<Value>(&stderr) {
        let code = v.pointer("/error/code").and_then(Value::as_str);
        let msg = v.pointer("/error/message").and_then(Value::as_str);
        return Err(match (code, msg) {
            (Some(c), Some(m)) => format!("{c}: {m}"),
            (Some(c), None) => c.to_string(),
            _ => stderr.trim().to_string(),
        });
    }
    Err(stderr.trim().to_string())
}

fn parse_agent(v: &Value) -> Option<AgentInfo> {
    let pane = v.get("pane_id")?.as_str()?.to_owned();
    let agent = v
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let status =
        AgentStatus::from_wire(v.get("agent_status").and_then(Value::as_str).unwrap_or(""));
    let state_change_seq = v
        .get("state_change_seq")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let session = v.get("agent_session").and_then(|s| {
        Some(SessionRef {
            agent: s.get("agent")?.as_str()?.to_owned(),
            kind: s.get("kind")?.as_str()?.to_owned(),
            value: s.get("value")?.as_str()?.to_owned(),
        })
    });
    Some(AgentInfo {
        pane,
        agent,
        status,
        state_change_seq,
        session,
    })
}

pub fn agent_get(bin: &OsStr, pane: &str) -> Result<AgentInfo, String> {
    let text = run_text(bin, &["agent", "get", pane])?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))?;
    v.pointer("/result/agent")
        .and_then(parse_agent)
        .ok_or_else(|| format!("no agent on {pane}"))
}

/// `kind == "path"` → the value verbatim. `kind == "id"` → the agent's own
/// transcript, which herdr does not surface a path for: for claude that is
/// `~/.claude/projects/<slug>/<uuid>.jsonl`, and the slug is derived from a
/// cwd we do not know here, so we scan. Ambiguity (the same uuid under two
/// project dirs) resolves to the most recently modified.
pub fn transcript_path(s: &SessionRef) -> Option<PathBuf> {
    if s.kind == "path" {
        let p = PathBuf::from(&s.value);
        return p.exists().then_some(p);
    }
    if s.kind != "id" {
        return None;
    }
    match s.agent.as_str() {
        "claude" => {
            let home = std::env::var_os("HOME")?;
            let projects = Path::new(&home).join(".claude/projects");
            let want = format!("{}.jsonl", s.value);
            let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
            for entry in std::fs::read_dir(&projects).ok()?.flatten() {
                let cand = entry.path().join(&want);
                let Ok(meta) = std::fs::metadata(&cand) else {
                    continue;
                };
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().is_none_or(|(b, _)| mtime > *b) {
                    best = Some((mtime, cand));
                }
            }
            best.map(|(_, p)| p)
        }
        // codex writes ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl and
        // reports kind=="path" when it reports anything, so there is
        // nothing to derive here.
        _ => None,
    }
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Pre-send gate + watermark. Refuses a non-resting agent: `--wait` would
/// match the *previous* turn's completion and we would attribute its text
/// to this thread. Captures `seq0` and the transcript byte offset BEFORE
/// anything is sent, so the harvest can be anchored exactly.
pub fn begin_turn(bin: &OsStr, pane: &str, prompt: &str) -> Result<Handle, PromptError> {
    let info = agent_get(bin, pane).map_err(PromptError::Cli)?;
    match info.status {
        AgentStatus::Blocked => return Err(PromptError::Blocked),
        s if !s.is_resting() => return Err(PromptError::Busy(s)),
        _ => {}
    }
    let transcript = info.session.as_ref().and_then(transcript_path);
    let offset = transcript.as_deref().map(file_len).unwrap_or(0);
    Ok(Handle {
        pane: pane.to_string(),
        seq0: info.state_change_seq,
        transcript,
        offset,
        prompt: prompt.to_string(),
    })
}

/// Blocking — call from the reply worker thread only. A `--wait` that
/// returns without advancing `state_change_seq` matched a stale state, and
/// the caller must NOT attribute any text to this turn.
pub fn prompt_wait(bin: &OsStr, h: &Handle, timeout_ms: u64) -> Result<AgentInfo, PromptError> {
    let timeout = timeout_ms.to_string();
    let args = [
        "agent",
        "prompt",
        &h.pane,
        &h.prompt,
        "--wait",
        "--until",
        "idle",
        "--until",
        "done",
        "--timeout",
        &timeout,
    ];
    let text = run_text(bin, &args).map_err(|e| {
        if e.starts_with("agent_blocked") {
            PromptError::Blocked
        } else if e.starts_with("agent_prompt_stalled") {
            PromptError::Stalled
        } else if e.starts_with("timeout") {
            PromptError::Timeout
        } else {
            PromptError::Cli(e)
        }
    })?;
    let v: Value =
        serde_json::from_str(&text).map_err(|e| PromptError::Cli(format!("bad json: {e}")))?;
    v.pointer("/result/agent")
        .and_then(parse_agent)
        .ok_or_else(|| PromptError::Cli("no agent in prompt reply".into()))
}

/// Did a genuinely new turn happen? `--wait` alone cannot tell us.
pub fn turn_advanced(h: &Handle, after: &AgentInfo) -> bool {
    after.state_change_seq > h.seq0 && after.status.is_resting()
}

// ---- reply capture --------------------------------------------------------

/// Primary capture. Reads the transcript from the pre-send watermark, anchors
/// on the first non-sidechain `user` record, then concatenates the `text`
/// blocks of every following non-sidechain `assistant` record, stopping at
/// the next genuine human prompt (a `user` record with no `toolUseResult`).
///
/// `isSidechain == true` records are subagent/Task transcripts written into
/// the SAME file — including them would splice a background agent's output
/// into the user's review thread.
pub fn harvest_transcript(h: &Handle) -> Option<String> {
    let path = h.transcript.as_deref()?;
    let bytes = read_from(path, h.offset)?;
    let mut out: Vec<String> = Vec::new();
    let mut anchored = false;
    for line in bytes.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // The final line can be a partial write while the agent streams.
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let is_tool_result = v.get("toolUseResult").is_some();
        match kind {
            "user" if !anchored => anchored = true,
            // A genuine new human prompt ends our turn; tool results do not.
            "user" if !is_tool_result => break,
            "assistant" if anchored => {
                let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
                    continue;
                };
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(t) = b.get("text").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        out.push(t.trim_end().to_string());
                    }
                }
            }
            _ => {}
        }
    }
    (!out.is_empty()).then(|| out.join("\n\n"))
}

fn read_from(path: &Path, offset: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Fallback only, and a lossy one: the alternate screen has no scrollback,
/// so anything above the viewport is already gone. Never `--source recent`
/// with `--lines` over the viewport (it hard-errors while working, and while
/// idle it physically scrolls the user's agent pane to harvest). Never
/// `--ansi` — the default `text` format already strips ANSI completely.
pub fn harvest_screen(bin: &OsStr, h: &Handle) -> Option<String> {
    let text = run_text(
        bin,
        &[
            "agent",
            "read",
            &h.pane,
            "--source",
            "detection",
            "--lines",
            "200",
        ],
    )
    .ok()?;
    Some(strip_chrome(&text, &h.prompt))
}

/// Drop everything at and above the echoed prompt, then the input box and
/// status chrome below it. These anchors are Claude-Code-specific and
/// version-fragile by nature — this path exists to degrade visibly, not to
/// be correct.
fn strip_chrome(screen: &str, prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    let lines: Vec<&str> = screen.lines().collect();
    let start = (!first.is_empty())
        .then(|| {
            lines.iter().rposition(|l| {
                let t = l.trim_start();
                t.starts_with('\u{276F}') && t.contains(first)
            })
        })
        .flatten()
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut out: Vec<String> = Vec::new();
    for line in &lines[start.min(lines.len())..] {
        let t = line.trim_end();
        let lead = t.trim_start();
        // The input box border ends the reply region.
        if lead.starts_with('\u{2500}') || lead.starts_with('\u{256D}') {
            break;
        }
        if lead.starts_with('\u{23BF}')      // ⎿ tool result
            || lead.starts_with('\u{273B}')  // ✻ spinner
            || lead.starts_with('\u{25EF}')  // ◯ workflow footer
            || lead.starts_with('\u{25FB}')  // ◻ todo
            || lead.starts_with('\u{23F5}')  // ⏵⏵ hint
            || lead.starts_with('\u{276F}')
        {
            continue;
        }
        let t = t.strip_prefix('\u{23FA}').map(str::trim_start).unwrap_or(t);
        out.push(t.to_string());
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_is_not_resting() {
        assert!(!AgentStatus::from_wire("something-new").is_resting());
        assert!(!AgentStatus::Working.is_resting());
        assert!(!AgentStatus::Blocked.is_resting());
        assert!(AgentStatus::Idle.is_resting());
        assert!(AgentStatus::Done.is_resting());
    }

    #[test]
    fn blocked_is_provably_undelivered_timeout_is_not() {
        assert_eq!(PromptError::Blocked.delivered(), Some(false));
        assert_eq!(
            PromptError::Busy(AgentStatus::Working).delivered(),
            Some(false)
        );
        assert_eq!(PromptError::Timeout.delivered(), None);
        assert_eq!(PromptError::Stalled.delivered(), None);
    }

    fn handle(prompt: &str) -> Handle {
        Handle {
            pane: "w1:p1".into(),
            seq0: 10,
            transcript: None,
            offset: 0,
            prompt: prompt.into(),
        }
    }

    #[test]
    fn turn_must_advance_the_seq() {
        let h = handle("q");
        let stale = AgentInfo {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            status: AgentStatus::Idle,
            state_change_seq: 10,
            session: None,
        };
        assert!(!turn_advanced(&h, &stale), "same seq = stale --wait match");
        let fresh = AgentInfo {
            state_change_seq: 11,
            ..stale.clone()
        };
        assert!(turn_advanced(&h, &fresh));
        let working = AgentInfo {
            state_change_seq: 12,
            status: AgentStatus::Working,
            ..stale.clone()
        };
        assert!(
            !turn_advanced(&h, &working),
            "still working = not a finished turn"
        );
    }

    /// Write a JSONL fixture and harvest it from byte 0.
    fn harvest_fixture(lines: &[&str]) -> Option<String> {
        let dir = std::env::temp_dir().join(format!(
            "gitview-agentio-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let got = harvest_transcript(&Handle {
            pane: "w1:p1".into(),
            seq0: 0,
            transcript: Some(path),
            offset: 0,
            prompt: "q".into(),
        });
        let _ = std::fs::remove_dir_all(&dir);
        got
    }

    fn asst(text: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn harvest_takes_only_this_turns_assistant_text() {
        let got = harvest_fixture(&[
            r#"{"type":"user","message":{"content":"my question"}}"#,
            &asst("first part"),
            &asst("second part"),
            r#"{"type":"user","message":{"content":"the NEXT question"}}"#,
            &asst("belongs to the next turn"),
        ]);
        assert_eq!(got.as_deref(), Some("first part\n\nsecond part"));
    }

    #[test]
    fn harvest_skips_sidechain_subagent_records() {
        // A Task/subagent writes into the SAME file; splicing its output
        // into the user's review thread is the bug this guards.
        let got = harvest_fixture(&[
            r#"{"type":"user","message":{"content":"q"}}"#,
            &asst("real reply"),
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"SUBAGENT NOISE"}]}}"#,
        ]);
        assert_eq!(got.as_deref(), Some("real reply"));
    }

    #[test]
    fn harvest_ignores_thinking_and_tool_use_blocks() {
        let got = harvest_fixture(&[
            r#"{"type":"user","message":{"content":"q"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"INTERNAL"},{"type":"tool_use","name":"Read","input":{}},{"type":"text","text":"visible answer"}]}}"#,
        ]);
        assert_eq!(got.as_deref(), Some("visible answer"));
    }

    #[test]
    fn harvest_does_not_end_on_a_tool_result_user_record() {
        let got = harvest_fixture(&[
            r#"{"type":"user","message":{"content":"q"}}"#,
            &asst("before the tool"),
            r#"{"type":"user","toolUseResult":{"ok":true},"message":{"content":"tool output"}}"#,
            &asst("after the tool"),
        ]);
        assert_eq!(
            got.as_deref(),
            Some("before the tool\n\nafter the tool"),
            "a tool-result user record is not a new human prompt"
        );
    }

    #[test]
    fn harvest_survives_a_partial_trailing_write() {
        let got = harvest_fixture(&[
            r#"{"type":"user","message":{"content":"q"}}"#,
            &asst("complete record"),
            r#"{"type":"assistant","message":{"content":[{"type":"te"#,
        ]);
        assert_eq!(got.as_deref(), Some("complete record"));
    }

    #[test]
    fn harvest_ignores_unknown_record_types() {
        let got = harvest_fixture(&[
            r#"{"type":"last-prompt","value":"x"}"#,
            r#"{"type":"file-history-snapshot"}"#,
            r#"{"type":"user","message":{"content":"q"}}"#,
            r#"{"type":"queue-operation"}"#,
            &asst("still found it"),
        ]);
        assert_eq!(got.as_deref(), Some("still found it"));
    }

    #[test]
    fn harvest_returns_none_when_nothing_follows_the_watermark() {
        assert_eq!(harvest_fixture(&[]), None);
        assert_eq!(
            harvest_fixture(&[r#"{"type":"user","message":{"content":"q"}}"#]),
            None,
            "a question with no reply yet is not an empty-string reply"
        );
    }

    #[test]
    fn screen_strip_drops_prompt_echo_and_chrome() {
        let screen = concat!(
            "some older output\n",
            "\u{276F} explain the auth flow\n",
            "\u{23FA} The auth flow starts in login.ts.\n",
            "  It then calls verify().\n",
            "  \u{23BF}  Read 40 lines\n",
            "\u{273B} Thinking… (12s)\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276F} \n",
        );
        let got = strip_chrome(screen, "explain the auth flow");
        assert_eq!(
            got,
            "The auth flow starts in login.ts.\n  It then calls verify()."
        );
    }

    #[test]
    fn screen_strip_without_echo_keeps_body() {
        let got = strip_chrome("\u{23FA} bare reply\n", "not on screen");
        assert_eq!(got, "bare reply");
    }
}
