//! The reply worker: delivers one thread's question to an agent and brings
//! its answer back.
//!
//! This runs off the UI thread for a blunt reason — `agent prompt --wait`
//! blocks for as long as the agent takes to think, which is minutes. Doing
//! that inline would freeze the diff pane mid-review.
//!
//! One thread per request, never a batch. A batch would deliver N questions
//! and get back one reply with no honest way to say which question it
//! answered; `agentio` goes to some trouble to attribute a reply to the exact
//! turn that caused it, and fanning it back out across N threads would throw
//! that away. Requests are therefore processed strictly in order: the agent
//! has to be resting before each one, so they could not overlap anyway.

use std::ffi::OsString;
use std::sync::mpsc::{self, Sender};

use crate::agentio;
use crate::thread::AgentRef;

/// How long to wait for one turn before giving up on capturing its reply.
/// The turn itself is unaffected — this only bounds how long we watch.
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;

/// Ask agent `pane` thread `id`'s question.
pub struct ReplyReq {
    pub pane: String,
    pub id: u64,
    pub prompt: String,
    /// Indices of the turns this prompt was composed from. Carried so the
    /// answer marks exactly those sent — the thread can gain or lose turns
    /// while the request waits behind a multi-minute one ahead of it, and
    /// marking "every Human turn" would then mark text nobody asked about.
    pub turns: Vec<usize>,
    pub timeout_ms: u64,
}

/// Whether the question can be asked again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Provably never reached the agent. Still pending; ask again freely.
    Pending,
    /// May be in front of the agent already. Asking again could duplicate
    /// work, so the user decides.
    Unknown,
}

/// What the worker reports back. Every request produces exactly one terminal
/// message (`Replied` or `Failed`), optionally preceded by `Delivered`.
pub enum ReplyMsg {
    /// The prompt was accepted by the agent surface. The thread moves to
    /// `Sent` now rather than when the turn finishes minutes later.
    Delivered {
        id: u64,
        turns: Vec<usize>,
        agent: Box<AgentRef>,
    },
    /// The turn finished and this is what the agent said.
    Replied { id: u64, text: String },
    /// Something went wrong. `turns` is carried so a `Pending` failure can
    /// undo exactly the marks this attempt made — `Delivered` is emitted
    /// optimistically, before herdr has actually accepted the prompt.
    Failed {
        id: u64,
        turns: Vec<usize>,
        err: String,
        retry: Retry,
    },
}

impl ReplyMsg {
    pub fn id(&self) -> u64 {
        match self {
            ReplyMsg::Delivered { id, .. }
            | ReplyMsg::Replied { id, .. }
            | ReplyMsg::Failed { id, .. } => *id,
        }
    }

    /// Terminal messages end the request; `Delivered` does not.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, ReplyMsg::Delivered { .. })
    }
}

/// Spawn the worker. Returns the request channel; the worker exits when it is
/// dropped, or when the event channel closes (the pane is gone).
pub fn spawn_reply_worker(tx: Sender<super::session::Event>, bin: OsString) -> Sender<ReplyReq> {
    let (req_tx, req_rx) = mpsc::channel::<ReplyReq>();
    std::thread::spawn(move || {
        while let Ok(req) = req_rx.recv() {
            let mut alive = true;
            run_one(&bin, &req, &mut |msg| {
                alive = tx.send(super::session::Event::Reply(msg)).is_ok();
                alive
            });
            if !alive {
                return; // the pane went away
            }
        }
    });
    req_tx
}

/// One request's whole lifecycle. Messages are emitted through `emit` as they
/// happen rather than returned at the end: the entire point of `Delivered` is
/// that the card stops saying "draft" while the agent thinks, and collecting
/// into a Vec would hold it back until the turn was already over.
///
/// `emit` returns false once the receiver is gone, which aborts the request.
fn run_one(bin: &OsString, req: &ReplyReq, emit: &mut dyn FnMut(ReplyMsg) -> bool) {
    let id = req.id;
    let turns = req.turns.clone();
    let fail = |err: String, retry: Retry| ReplyMsg::Failed {
        id,
        turns: turns.clone(),
        err,
        retry,
    };

    // Resolve the agent first so `Delivered` can name it. Also the cheapest
    // way to fail early on a pane that has no agent at all.
    let agent = match agentio::agent_get(bin, &req.pane) {
        Ok(info) => AgentRef {
            pane: req.pane.clone(),
            agent: info.agent.clone(),
            session: info.session.as_ref().map(|s| s.value.clone()),
            session_kind: info.session.as_ref().map(|s| s.kind.clone()),
        },
        // `agent get` is a read: nothing was sent.
        Err(e) => {
            emit(fail(e, Retry::Pending));
            return;
        }
    };

    // `begin_turn` refuses a non-resting agent rather than interleaving with
    // a turn already in flight, and records the transcript watermark that
    // makes the reply attributable. It only reads, so every failure here is
    // provably pre-send.
    let handle = match agentio::begin_turn(bin, &req.pane, &req.prompt) {
        Ok(h) => h,
        Err(e) => {
            emit(fail(describe(&e), Retry::Pending));
            return;
        }
    };

    // Optimistic: herdr has not accepted the prompt yet — `prompt_wait` is
    // what actually delivers it. Saying so now is worth it for the badge,
    // and a `Pending` failure below undoes exactly these turns.
    if !emit(ReplyMsg::Delivered {
        id,
        turns: turns.clone(),
        agent: Box::new(agent),
    }) {
        return;
    }

    let after = match agentio::prompt_wait(bin, &handle, req.timeout_ms) {
        Ok(info) => info,
        Err(e) => {
            // Blocked/Busy are refusals: herdr rejected the prompt without
            // delivering it, so the turns go back to pending.
            let retry = match e.delivered() {
                Some(false) => Retry::Pending,
                _ => Retry::Unknown,
            };
            emit(fail(describe(&e), retry));
            return;
        }
    };

    // The agent must have both moved on from the state we sent into AND come
    // back to rest. Without this an already-working agent looks like it
    // answered instantly, and we would harvest the previous turn's text.
    if !agentio::turn_advanced(&handle, &after) {
        emit(fail(
            "the agent never finished this turn".to_string(),
            Retry::Unknown,
        ));
        return;
    }

    // The transcript is exact but only exists for agents whose session we can
    // resolve. Reading the pane is lossy — it cannot recover a reply taller
    // than the viewport — but a lossy answer beats discarding a finished one.
    let text = agentio::harvest_transcript(&handle)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| agentio::harvest_screen(bin, &handle))
        .filter(|t| !t.trim().is_empty());
    match text {
        Some(text) => emit(ReplyMsg::Replied { id, text }),
        // Delivered and completed, but nothing readable came back. Say so
        // rather than inventing an empty reply turn.
        None => emit(fail(
            "the agent replied, but its answer could not be read".to_string(),
            Retry::Unknown,
        )),
    };
}

fn describe(e: &agentio::PromptError) -> String {
    use agentio::PromptError as P;
    match e {
        P::Blocked => "the agent is waiting on a prompt of its own — clear it first".to_string(),
        P::Busy(s) => format!("the agent is {} — try again when it settles", s.as_str()),
        P::Stalled => "the agent accepted the text but never started working".to_string(),
        P::Timeout => "timed out waiting for the agent".to_string(),
        P::Cli(m) => m.clone(),
    }
}

/// The prompt for one thread: where it hangs, what was asked, and the code it
/// is about. Continuation lines are indented so the anchor stays readable.
pub fn compose_prompt(repo_root: &std::path::Path, thread: &crate::thread::Thread) -> String {
    let a = &thread.anchor;
    let mut msg = String::new();
    // Absolute, not repo-relative: the picked agent may be sitting in a
    // different repo — herdr shows a cwd column precisely because that
    // happens — where a bare `src/foo.rs` silently resolves to another file.
    let path = repo_root.join(&a.file);
    let where_ = if a.end == 0 {
        format!("{}", path.display())
    } else {
        format!("{}:{}-{}", path.display(), a.start, a.end)
    };
    for turn in thread.unsent() {
        msg.push_str(&format!(
            "{where_} — {}\n",
            indent_continuations(&turn.text)
        ));
    }
    if !a.snippet.is_empty() {
        // Size the fence to the content: a diff of a Markdown file routinely
        // contains ``` lines, and a fixed three-backtick fence would be
        // closed early by the snippet's own text.
        let fence = "`".repeat(longest_backtick_run(&a.snippet).max(2) + 1);
        msg.push_str(&format!("{fence}diff\n"));
        msg.push_str(&a.snippet);
        if !a.snippet.ends_with('\n') {
            msg.push('\n');
        }
        msg.push_str(&format!("{fence}\n"));
    }
    msg
}

fn longest_backtick_run(s: &str) -> usize {
    let (mut best, mut run) = (0usize, 0usize);
    for c in s.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

fn indent_continuations(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").to_string();
    lines.fold(first, |mut acc, l| {
        acc.push_str("\n  ");
        acc.push_str(l);
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::{Anchor, Author, Thread};
    use std::path::{Path, PathBuf};

    fn thread_with(text: &str, snippet: &str) -> Thread {
        let mut t = Thread::new(
            1,
            Anchor {
                file: PathBuf::from("src/a.rs"),
                start: 10,
                end: 12,
                cached: false,
                snippet: snippet.to_string(),
                blob: None,
                head_sha: None,
            },
            text.to_string(),
        );
        t.state = crate::thread::ThreadState::Draft;
        t
    }

    #[test]
    fn a_prompt_names_the_lines_and_fences_the_code() {
        let t = thread_with("why is this safe?", "+let x = y.unwrap();\n");
        let p = compose_prompt(Path::new("/repo"), &t);
        assert!(
            p.contains("/repo/src/a.rs:10-12 — why is this safe?"),
            "{p}"
        );
        assert!(p.contains("```diff\n+let x = y.unwrap();\n```"), "{p}");
    }

    #[test]
    fn a_whole_file_thread_names_only_the_file() {
        let mut t = thread_with("general question", "");
        t.anchor.start = 0;
        t.anchor.end = 0;
        let p = compose_prompt(Path::new("/repo"), &t);
        assert!(p.starts_with("/repo/src/a.rs — general question"), "{p}");
        assert!(!p.contains("a.rs:"), "no bogus line range: {p}");
    }

    #[test]
    fn only_unsent_turns_are_asked() {
        let mut t = thread_with("first question", "");
        t.mark_sent(AgentRef {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            session: None,
            session_kind: None,
        });
        t.push(Author::Agent, "an answer".into());
        t.push(Author::Human, "follow-up".into());
        let p = compose_prompt(Path::new("/repo"), &t);
        assert!(p.contains("follow-up"), "{p}");
        assert!(
            !p.contains("first question"),
            "a delivered turn must not be re-asked: {p}"
        );
        assert!(!p.contains("an answer"), "the agent's own words: {p}");
    }

    #[test]
    fn multi_line_questions_keep_their_shape_under_the_anchor() {
        let t = thread_with("line one\nline two", "");
        let p = compose_prompt(Path::new("/repo"), &t);
        assert!(p.contains("— line one\n  line two"), "{p}");
    }

    /// A diff of a Markdown file routinely contains ``` lines. A fixed
    /// three-backtick fence would be closed by the snippet's own text, so
    /// everything after it reads as prose — or as instructions.
    #[test]
    fn a_snippet_containing_a_fence_gets_a_longer_one() {
        let t = thread_with("why?", "+```rust\n+let x = 1;\n+```\n");
        let p = compose_prompt(Path::new("/repo"), &t);
        assert!(
            p.contains("````diff"),
            "fence must outgrow the content: {p}"
        );
        assert!(
            p.ends_with("````\n"),
            "and close with the same width: {p:?}"
        );
        // The snippet's own fences survive intact inside it.
        assert!(p.contains("+```rust"), "{p}");
    }

    #[test]
    fn a_snippet_without_a_trailing_newline_still_closes_its_fence() {
        let t = thread_with("q", "+no trailing newline");
        let p = compose_prompt(Path::new("/repo"), &t);
        assert!(p.ends_with("```\n"), "{p:?}");
        assert!(
            !p.contains("newline```"),
            "fence must start its own line: {p}"
        );
    }
}
