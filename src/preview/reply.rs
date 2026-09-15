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
    pub timeout_ms: u64,
}

/// What the worker reports back. Every request produces exactly one terminal
/// message (`Replied` or `Failed`), optionally preceded by `Delivered`.
pub enum ReplyMsg {
    /// The prompt reached the agent. The thread can move to `Sent` now,
    /// rather than after the turn finishes minutes later.
    Delivered { id: u64, agent: Box<AgentRef> },
    /// The turn finished and this is what the agent said.
    Replied { id: u64, text: String },
    /// Something went wrong. `delivered` distinguishes "the agent never got
    /// it" (safe to resend) from "we don't know" (resending may duplicate).
    Failed {
        id: u64,
        err: String,
        delivered: Option<bool>,
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
}

/// Spawn the worker. Returns the request channel; the worker exits when it is
/// dropped, or when the event channel closes (the pane is gone).
pub fn spawn_reply_worker(tx: Sender<super::session::Event>, bin: OsString) -> Sender<ReplyReq> {
    let (req_tx, req_rx) = mpsc::channel::<ReplyReq>();
    std::thread::spawn(move || {
        while let Ok(req) = req_rx.recv() {
            for msg in run_one(&bin, &req) {
                if tx.send(super::session::Event::Reply(msg)).is_err() {
                    return; // the pane went away
                }
            }
        }
    });
    req_tx
}

/// One request's whole lifecycle, as the messages it produces.
fn run_one(bin: &OsString, req: &ReplyReq) -> Vec<ReplyMsg> {
    let id = req.id;

    // Resolve the agent first so a `Delivered` can name it. This is also the
    // cheapest way to fail early on a pane that has no agent at all.
    let agent = match agentio::agent_get(bin, &req.pane) {
        Ok(info) => AgentRef {
            pane: req.pane.clone(),
            agent: info.agent.clone(),
            session: info.session.as_ref().map(|s| s.value.clone()),
            session_kind: info.session.as_ref().map(|s| s.kind.clone()),
        },
        Err(e) => {
            return vec![ReplyMsg::Failed {
                id,
                err: e,
                // Nothing was sent: `agent get` is a read.
                delivered: Some(false),
            }];
        }
    };

    // `begin_turn` refuses a non-resting agent rather than interleaving with
    // a turn already in flight, and records the transcript watermark that
    // makes the reply attributable.
    let handle = match agentio::begin_turn(bin, &req.pane, &req.prompt) {
        Ok(h) => h,
        Err(e) => {
            return vec![ReplyMsg::Failed {
                id,
                err: describe(&e),
                delivered: e.delivered(),
            }];
        }
    };

    let mut out = vec![ReplyMsg::Delivered {
        id,
        agent: Box::new(agent),
    }];

    let after = match agentio::prompt_wait(bin, &handle, req.timeout_ms) {
        Ok(info) => info,
        Err(e) => {
            out.push(ReplyMsg::Failed {
                id,
                err: describe(&e),
                delivered: e.delivered(),
            });
            return out;
        }
    };

    // The agent must have both moved on from the state we sent into AND come
    // back to rest. Without this an already-working agent looks like it
    // answered instantly, and we would harvest the previous turn's text.
    if !agentio::turn_advanced(&handle, &after) {
        out.push(ReplyMsg::Failed {
            id,
            err: "the agent never finished this turn".to_string(),
            delivered: Some(true),
        });
        return out;
    }

    match agentio::harvest_transcript(&handle) {
        Some(text) if !text.trim().is_empty() => out.push(ReplyMsg::Replied { id, text }),
        // Delivered and completed, but nothing readable came back. Say so
        // rather than inventing an empty reply turn.
        _ => out.push(ReplyMsg::Failed {
            id,
            err: "the agent replied, but its answer could not be read".to_string(),
            delivered: Some(true),
        }),
    }
    out
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
pub fn compose_prompt(thread: &crate::thread::Thread) -> String {
    let a = &thread.anchor;
    let mut msg = String::new();
    let where_ = if a.end == 0 {
        format!("{}", a.file.display())
    } else {
        format!("{}:{}-{}", a.file.display(), a.start, a.end)
    };
    for turn in thread.unsent() {
        msg.push_str(&format!(
            "{where_} — {}\n",
            indent_continuations(&turn.text)
        ));
    }
    if !a.snippet.is_empty() {
        msg.push_str("```diff\n");
        msg.push_str(&a.snippet);
        if !a.snippet.ends_with('\n') {
            msg.push('\n');
        }
        msg.push_str("```\n");
    }
    msg
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
    use std::path::PathBuf;

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
        let p = compose_prompt(&t);
        assert!(p.contains("src/a.rs:10-12 — why is this safe?"), "{p}");
        assert!(p.contains("```diff\n+let x = y.unwrap();\n```"), "{p}");
    }

    #[test]
    fn a_whole_file_thread_names_only_the_file() {
        let mut t = thread_with("general question", "");
        t.anchor.start = 0;
        t.anchor.end = 0;
        let p = compose_prompt(&t);
        assert!(p.starts_with("src/a.rs — general question"), "{p}");
        assert!(!p.contains(':'), "no bogus line range: {p}");
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
        let p = compose_prompt(&t);
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
        let p = compose_prompt(&t);
        assert!(p.contains("— line one\n  line two"), "{p}");
    }

    #[test]
    fn a_snippet_without_a_trailing_newline_still_closes_its_fence() {
        let t = thread_with("q", "+no trailing newline");
        let p = compose_prompt(&t);
        assert!(p.ends_with("```\n"), "{p:?}");
        assert!(
            !p.contains("newline```"),
            "fence must start its own line: {p}"
        );
    }
}
