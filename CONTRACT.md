# herdr-gitview `threads` — implementation contract

Repo: `/Users/keyur/Code/herdr-gitview` (branch `threads`). Verified against the working tree; `herdr 0.9.0` confirmed on this machine, `agent prompt --wait/--until/--timeout` help text confirmed verbatim.

---

## 1. `src/thread.rs` — paste-ready types

```rust
//! Persistent threaded review conversations, anchored to a code change.
//!
//! The preview pane is the sole owner and the sole id allocator; the list
//! mirrors a projection (`ipc::NoteMeta`) and acts strictly by id.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STORE_VERSION: u32 = 1;

/// Unix epoch seconds. Integer on purpose: every type below derives `Eq`,
/// so no float may ever enter this module (the wire type forbids it too).
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    #[default]
    Human,
    Agent,
    /// Delivery/capture diagnostics rendered inline ("send failed: …").
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    /// Has at least one unsent Human turn.
    #[default]
    Draft,
    /// Delivered; waiting on the agent.
    Sent,
    /// An Agent turn landed.
    Answered,
    /// Delivery or capture failed. Text may or may not have reached the
    /// agent — never auto-resend from this state.
    Failed,
    /// User closed it. Never re-sent, hidden by default in the notes view.
    Resolved,
}

impl ThreadState {
    /// Title badge. Must carry a non-default `Style` at the call site —
    /// `card_box` rewrites any `Style::default()` span to the body colour.
    pub fn badge(self) -> &'static str {
        match self {
            ThreadState::Draft => "draft",
            ThreadState::Sent => "sent…",
            ThreadState::Answered => "replied",
            ThreadState::Failed => "failed",
            ThreadState::Resolved => "done",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub author: Author,
    pub text: String,
    pub at: i64,
    /// Set once this turn's text has been included in a delivery. This is the
    /// only thing preventing a second `p` from re-sending the whole review.
    #[serde(default)]
    pub sent: bool,
}

/// Where a thread hangs. `start/end` are NEW-side line numbers; `0-0` is a
/// whole-file thread. `snippet` is the only rewrite-resistant signal we have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub file: PathBuf,
    pub start: u32,
    pub end: u32,
    /// The staged side the thread was written against, so re-showing picks
    /// the same diff (regression: tests/scenarios.rs:395).
    pub cached: bool,
    /// Selected diff lines, `-`/`+`/space prefixed, ≤40 lines.
    #[serde(default)]
    pub snippet: String,
    /// `git hash-object` of the new side at creation, for re-anchoring.
    #[serde(default)]
    pub blob: Option<String>,
    /// HEAD when the thread was created — the "as-of" stamp.
    #[serde(default)]
    pub head_sha: Option<String>,
}

/// The agent a thread is in conversation with. Keyed on the session value,
/// NOT the pane id: pane ids are reassigned by `pane move` and the agent name
/// is cleared when the occupant exits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub pane: String,
    pub agent: String,
    /// `agent_session.value`.
    #[serde(default)]
    pub session: Option<String>,
    /// `agent_session.kind`: "id" | "path".
    #[serde(default)]
    pub session_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: u64,
    pub anchor: Anchor,
    /// Never empty for a persisted thread: turns[0] is the opening Human turn.
    pub turns: Vec<Turn>,
    #[serde(default)]
    pub state: ThreadState,
    #[serde(default)]
    pub agent: Option<AgentRef>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

impl Thread {
    pub fn last(&self) -> Option<&Turn> {
        self.turns.last()
    }
    /// The list-row preview line: the newest turn.
    pub fn preview(&self) -> &str {
        self.turns.last().map(|t| t.text.as_str()).unwrap_or("")
    }
    pub fn has_unsent(&self) -> bool {
        self.state != ThreadState::Resolved
            && self.turns.iter().any(|t| t.author == Author::Human && !t.sent)
    }
    pub fn unsent(&self) -> impl Iterator<Item = &Turn> {
        self.turns.iter().filter(|t| t.author == Author::Human && !t.sent)
    }
    /// The last Human turn that may still be rewritten in place.
    pub fn editable_turn(&self) -> Option<usize> {
        self.turns
            .iter()
            .rposition(|t| t.author == Author::Human && !t.sent)
    }
    pub fn push(&mut self, author: Author, text: String) {
        self.turns.push(Turn { author, text, at: now_epoch(), sent: false });
        self.updated_at = now_epoch();
    }
    pub fn mark_sent(&mut self, agent: AgentRef) {
        for t in self.turns.iter_mut().filter(|t| t.author == Author::Human) {
            t.sent = true;
        }
        self.agent = Some(agent);
        self.state = ThreadState::Sent;
        self.updated_at = now_epoch();
    }
    pub fn meta(&self) -> crate::ipc::NoteMeta {
        crate::ipc::NoteMeta {
            id: self.id,
            file: self.anchor.file.clone(),
            start: self.anchor.start,
            end: self.anchor.end,
            text: self.preview().to_string(),
            cached: self.anchor.cached,
            turns: self.turns.len() as u32,
            state: self.state,
            last_author: self.last().map(|t| t.author).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadStore {
    pub version: u32,
    /// Resolved `git rev-parse --show-toplevel`, for diagnosing a key clash.
    pub repo: PathBuf,
    /// Branch-scope baseline this store was last written against.
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub merge_base: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
    /// Id watermark. MUST be `> max(thread.id)` at all times, including
    /// immediately after load — every list→preview command is by id, so a
    /// collision silently retargets edit/delete/focus at the wrong thread.
    pub next_id: u64,
    pub threads: Vec<Thread>,
}

impl ThreadStore {
    pub fn empty(repo: &Path) -> ThreadStore {
        ThreadStore {
            version: STORE_VERSION,
            repo: repo.to_path_buf(),
            base_ref: None,
            merge_base: None,
            head_sha: None,
            next_id: 1,
            threads: Vec::new(),
        }
    }

    /// Never fatal, but never silent either: a decode failure returns the
    /// error string so the caller can flash it. Unlike
    /// `orchestrate::read_state_file` (:611) this must NOT `.ok()` away a
    /// failed load — discarding a review is much worse than discarding a
    /// pane layout.
    pub fn load(path: &Path, repo: &Path) -> (ThreadStore, Option<String>) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (ThreadStore::empty(repo), None);
            }
            Err(e) => return (ThreadStore::empty(repo), Some(e.to_string())),
        };
        match serde_json::from_slice::<ThreadStore>(&bytes) {
            Ok(mut s) => {
                let hi = s.threads.iter().map(|t| t.id).max().unwrap_or(0);
                s.next_id = s.next_id.max(hi + 1);
                (s, None)
            }
            Err(e) => {
                let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
                (ThreadStore::empty(repo), Some(format!("thread store unreadable: {e}")))
            }
        }
    }

    /// tmp + rename, so a crash mid-write never truncates the review.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    pub fn get(&self, id: u64) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }
    pub fn get_mut(&mut self, id: u64) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| t.id == id)
    }
    /// The threads a `p` would actually send.
    pub fn sendable(&self) -> Vec<u64> {
        self.threads.iter().filter(|t| t.has_unsent()).map(|t| t.id).collect()
    }
}

/// `$HERDR_PLUGIN_STATE_DIR/threads/<view-key>.json`.
///
/// The key is the IPC socket's file stem, which is already unique per *view*
/// (`<repo_hash>.tab` or `<repo_hash>.<tab_key>`, orchestrate.rs:568-586).
/// That gives per-view isolation for free: a repo can host one tab view plus
/// one sidebar per host tab (orchestrate.rs:638 `all_states`), each a separate
/// preview process with its own store — a single per-repo file would be
/// written concurrently by all of them and the id watermarks would collide.
/// Standalone (no socket) falls back to the repo hash.
///
/// NOTE: this lives in a SIBLING of `views/`, never inside it — filename
/// length under `views/` is load-bearing for macOS `sockaddr_un.sun_path`
/// (104 incl. NUL; orchestrate.rs:687 + the test at :1034).
pub fn store_path(socket: Option<&Path>, repo_root: &Path) -> PathBuf {
    let dir = crate::logx::state_dir().join("threads");
    let key = socket
        .and_then(|s| s.file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| crate::orchestrate::repo_hash(repo_root));
    dir.join(format!("{key}.json"))
}
```

---

## 2. On-disk schema

**Path** (exact): `$HERDR_PLUGIN_STATE_DIR/threads/<view-key>.json`
- `state_dir()` = `logx.rs:20` → `$HERDR_PLUGIN_STATE_DIR`, else `~/.local/state/herdr-gitview`.
- `<view-key>` = `env.socket.file_stem()` (e.g. `4f2a1c8e9b0d3776.tab`, `4f2a1c8e9b0d3776.3f2c8d91`).
- Standalone fallback: `repo_hash(repo.root)` = `format!("{:016x}", fnv1a(root.to_string_lossy()))` (orchestrate.rs:681).
- Sidecars: `<view-key>.json.tmp` (write staging), `<view-key>.json.corrupt` (quarantined bad load).

**Repo + baseline key** — the repo identity is the *checkout path hash* (a moved repo or a second git worktree gets a fresh store; that is correct for worktree-anchored threads). The baseline is stored once at store level and per-thread in `Anchor.head_sha`:

```json
{
  "version": 1,
  "repo": "/Users/keyur/Code/herdr-gitview",
  "base_ref": "origin/main",
  "merge_base": "9f1c0ab3c7d14e2f5b8a6d0e3c9f7a21b4d5e6f0",
  "head_sha": "3c7d14e2f5b8a6d0e3c9f7a21b4d5e6f09f1c0ab",
  "next_id": 4,
  "threads": [
    {
      "id": 3,
      "anchor": {
        "file": "src/preview/session.rs",
        "start": 383, "end": 404,
        "cached": false,
        "snippet": "-    fn deliver_notes(&self, pane: &str, submit: bool) -> Result<String> {\n+    fn deliver(&mut self, ...) -> Result<Handle> {\n",
        "blob": "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
        "head_sha": "3c7d14e2f5b8a6d0e3c9f7a21b4d5e6f09f1c0ab"
      },
      "turns": [
        { "author": "human", "text": "this takes &self — how do you mark the thread sent?", "at": 1789234567, "sent": true },
        { "author": "agent", "text": "Changed the signature to &mut self and moved the transition to poll_popups.", "at": 1789234712, "sent": false }
      ],
      "state": "answered",
      "agent": {
        "pane": "w1:p1",
        "agent": "claude",
        "session": "27ce87bf-8599-420a-b3ee-56b71392d92f",
        "session_kind": "id"
      },
      "created_at": 1789234501,
      "updated_at": 1789234712
    }
  ]
}
```

`version` is checked on load; `!= STORE_VERSION` → quarantine + flash, never silent-drop. Every field added after v1 gets `#[serde(default)]`.

---

## 3. Ordered edits, `file:line -> change`

Do them in this order — the compiler walks you through most of it (`list/app.rs:307` has no catch-all, both `on_ipc` matches are exhaustive).

### Phase A — model + persistence (items 1, 6)

| file:line | change |
|---|---|
| `src/lib.rs:1-17` | add `pub mod thread;` and `pub mod agentio;` |
| `src/orchestrate.rs:672` | `fn fnv1a` → `pub(crate) fn fnv1a` |
| `src/orchestrate.rs:681` | `fn repo_hash` → `pub(crate) fn repo_hash` |
| `src/ipc.rs:107-117` | `NoteMeta` gains `#[serde(default)] pub turns: u32`, `#[serde(default)] pub state: crate::thread::ThreadState`, `#[serde(default)] pub last_author: crate::thread::Author`. Keep the `type`/`notes`/`text` names — redefine `text` as *the newest turn's text*. Keep the `Eq` derive (no floats). **Do not put `Vec<Turn>` here** — see §6. |
| `src/ipc.rs:88-90` | `Ready` → `Ready { #[serde(default)] proto: u32 }`. It is the only handshake hook; add it now so the *next* breaking change has somewhere to negotiate. |
| `src/preview/app.rs:37-52` | delete `struct Note`; `use crate::thread::{Anchor, Author, Thread, ThreadState, ThreadStore, Turn};` |
| `src/preview/app.rs:106` | `pub notes: Vec<Note>` → `pub store: ThreadStore` (+ `pub fn threads(&self) -> &[Thread]`) |
| `src/preview/app.rs:108` | `pending_note: Option<Note>` → `pending: Option<Pending>` where `struct Pending { anchor: Anchor, target: Target }`, `enum Target { New, Edit { id: u64, turn: usize }, Reply(u64) }` |
| `src/preview/app.rs:134,175` | delete `next_note_id`; ids come from `store.alloc_id()` |
| `src/preview/app.rs:~160` (`PreviewApp::new`) | take `store_path: PathBuf`; `let (store, err) = ThreadStore::load(&path, &repo.root)`; stash `err` into `flash`. Bump `notes_rev` to 1 so the first `tick` broadcasts the loaded threads (session.rs:222 is the ONLY broadcast trigger). |
| `src/preview/app.rs:955-988` | `finish_annotate`/`edit_note`/`delete_note` operate on `store`; each ends with `self.notes_rev += 1; self.persist(); self.rebuild();`. Add `fn persist(&mut self)` = `store.save(&self.store_path)`, on error `flash` once. `clear_notes` (:967) → keep as an explicit "clear all resolved" or delete it. |
| `src/preview/app.rs:795` | **bug fix, do it before persisting**: `numbers.push(new.or(old).unwrap_or(0))` stores an OLD-side line number in a field everything downstream treats as NEW-side. Deletion-only selections must either be skipped or recorded with a side marker — persisting the current behaviour makes the error permanent. |
| `src/preview/app.rs:776-827` | `begin_annotate` builds `Anchor { .., blob: repo.hash_object(file), head_sha: repo.head_sha() }` and `Pending { anchor, target: Target::New }` |
| `src/preview/session.rs:222-238` | projection becomes `self.app.store.threads.iter().map(Thread::meta).collect()` |
| `src/preview/mod.rs:44` | `PreviewApp::new(cfg, repo, keys)` → pass `thread::store_path(env.socket.as_deref(), &repo.root)` (resolve `env` before `app`) |
| `src/list/ui.rs:350-370` | `note_row` gains a third line: state badge + `turns` count. |
| `src/list/rows.rs:103` | `ListRow::Note(_) => 2` → `3` (or collapse-aware). **Must change in the same commit** as `note_row` or `row_at` (list/app.rs:416) and `list_offset` desync. |
| `src/list/ui.rs:147` | `{} notes` → `{} threads` |
| `src/list/app.rs:686-730, 817` | add `CursorId::Note(u64)`, handle in `cursor_identity`/`restore_cursor` |
| `src/list/session.rs:297-306` | wrap `self.app.notes = notes` in `cursor_identity`/`restore_cursor`. Today snapshots only change from a list-side action; the moment an agent reply appends a turn asynchronously the cursor silently retargets and the next debounced hover flush sends `FocusNote`/`DeleteNote` for the wrong id. |
| `tests/preview.rs:297-318`, `tests/render.rs:189-199`, `tests/scenarios.rs:352` | struct literals — fix to compile |

### Phase B — stop clearing on send (item 2)

| file:line | change |
|---|---|
| `src/preview/session.rs:292` | delete `self.app.clear_notes();` |
| `src/preview/session.rs:385` | `for note in &self.app.notes` → iterate `store.sendable()` only, and emit only `Thread::unsent()` turns. Without this every subsequent `p` re-sends every answered thread. |
| `src/preview/session.rs:388-404` | payload gains a stable marker per thread: `[gitview:thread:{id}] {file}:{start}-{end}` — nothing today lets a reply be attributed back. |
| `src/preview/session.rs:531` | `indent_continuations` → per-turn role prefixes (`you:` / `agent:`) |
| `src/preview/app.rs:56` | `PopupReq::PickAgent` → `PickAgent { threads: Vec<u64> }` so the picker text and the post-send transition agree on what was sent |
| `src/preview/app.rs:619-624`, `src/preview/session.rs:189`, `src/list/app.rs:337`, `src/list/app.rs:553` | four `is_empty()` gates that meant "nothing to send" — replace with `store.sendable().is_empty()` (`:553` `toggle_notes_view` must instead test `threads.is_empty()`: opening the view to read answered threads is the point) |
| `src/preview/ui.rs:182-186`, `src/preview/session.rs:262` | `send ({})` and `GITVIEW_ASK_TEXT` count `sendable().len()`, not total |
| `src/list/session.rs:299-305` | the auto-exit on an empty snapshot now only fires on "last thread deleted" — still correct, but it was the list's *only* post-send feedback. `note_row`'s state badge (Phase A) is now load-bearing. |
| `tests/scenarios.rs:309-357` | encodes disposable-on-send + empty-view; rewrite |

### Phase C — nested thread card (item 7)

| file:line | change |
|---|---|
| `src/preview/card.rs:19-25` | `pub note: Option<u64>` → `pub kind: CardKind` (see §4). Keeping the `Option` and giving the composer a thread id silently leaves `composer_span == None` and makes `scroll_to_composer` a no-op. |
| `src/preview/card.rs:77-89` | add `thread_card`; `note_card` stays for the whole-file/degenerate case |
| `src/preview/card.rs:194-202` | title `fill = box_w - 3 - label.width()` assumes one elided string. Add `card_box_titled(title_spans, …)` computing `fill` from summed span widths, for the state badge. |
| `src/preview/card.rs:214-220` | body loop rewrites any `style == Style::default()` span to the body colour — **every role prefix and badge span must carry a non-default style** or it loses its colour with no error |
| `src/preview/app.rs:309` | drop `Some(n.id) != editing`: a thread must stay visible while you reply into it |
| `src/preview/app.rs:328-347` | composer card is built standalone only for `Target::New`; for `Edit`/`Reply` it is nested and `thread_card` returns the reply box's sub-range |
| `src/preview/app.rs:375-378` | `match card.note` → `match card.kind`; `composer_span` = the nested sub-range, not the whole card |
| `src/preview/app.rs:847` | `composer_width()` must return the width the *nested* box renders at, or TextArea up/down/home/end land on the wrong row |
| `src/preview/app.rs:124, 448, 678, 757` | `card_lines: Vec<usize>` is linear-scanned on every cursor move, click and restyle. Three-line cards make that free; thread cards do not. Switch to a sorted `Vec<usize>` + `binary_search`, or `Vec<Range<usize>>` + prefix sums. |
| `src/preview/app.rs:544-562` | `keep_cursor_visible` clamps in ROW space then `snap_off_card` pushes back out in LINE space. A card taller than `viewport_h` leaves the cursor off-screen with no visible cursor. **This is the sharpest new breakage** — 3-line cards can never trigger it. Add a collapse default + a guard: if every line in the clamped window is a card line, park the cursor and scroll by rows. |
| `src/preview/app.rs:1005-1013` | `scroll_to_note`'s `row.saturating_sub(3)` puts a long thread's newest turn below the fold. Use `scroll_to_composer`'s two-sided clamp, biased to the *bottom*. |
| `src/preview/compose.rs:14-18` | `editing: Option<u64>` → `target: Target` (there is no turn index today, so "edit turn N" is unrepresentable) |
| `src/preview/compose.rs:68-71` | empty `finish()` must not destroy an existing thread when a reply is abandoned |
| `src/preview/app.rs:853-872` | `commit_composer` gains a third branch: `Target::Reply(id) => append Human turn` |
| `src/preview/app.rs:884-894` | `begin_edit_note` selects `thread.editable_turn()` and refuses when `state != Draft` |
| `src/keymap.rs:7,43` | add `Action::Reply` (`"reply"`, `&["shift+r"]`), `Action::ToggleThread` (`"toggle_thread"`, `&["z"]`), `Action::Resolve` (`"resolve"`, `&["shift+x"]`). Taken today: `j k g G enter ctrl+d ctrl+u home end w tab s u x c l v a p n d r ? q esc` + arrows. `R`/`z`/`X` are free. **A collision makes `Keymap::build` bail and the `.expect("default keymap is valid")` at preview/mod.rs:40 / list/mod.rs:108 PANIC both panes at startup.** |
| `src/ipc.rs:70` | add `ComposeReply { id: u64 }`, `ResolveThread { id: u64 }` |
| `src/list/app.rs:307-345`, `src/preview/app.rs:593-636` | arms for the three new actions (`list/app.rs` is exhaustive and will fail to compile; `preview/app.rs` ends in `_ => {}` and will silently no-op) |
| `src/list/ui.rs:19`, `:520-524`, `assets/example-config.toml:44` | help modal row, `Mode::Notes` footer pairs, and a commented binding line (that file is already missing 7 shipped actions — backfill while you are there) |

### Phase D — send via agent surface + reply capture (items 3, 4)

| file:line | change |
|---|---|
| `src/popup.rs:199-249` | return a struct, not a 5-tuple; add `agent_session` (kind+value) and `revision` — the loop at `:236` already parses the exact JSON record that contains them |
| `src/herdr_cli.rs:129-135` | `run_json` discards stderr. herdr puts `agent_blocked` / `agent_prompt_stalled` / `timeout` / `agent_not_idle` on **stderr with exit 1** — every diagnosable failure is currently swallowed. Add `run_text(bin, args) -> Result<String, String>` returning stderr on failure. |
| `src/hostenv.rs:10-20` | add `pub herdr_socket: Option<PathBuf>` from `HERDR_SOCKET_PATH` (optional; buys `truncated` on `agent.read`, which the CLI drops) |
| `src/preview/session.rs:34-49` | add `Event::Reply(crate::preview::reply::ReplyMsg)` |
| `src/preview/session.rs:60-81` | `Session::new` spawns `reply::spawn_reply_worker(tx.clone(), env.herdr_bin.clone())` alongside the diff worker |
| `src/preview/session.rs:383-423` | `fn deliver_notes(&self, …) -> Result<String>` → `fn dispatch(&mut self, pane: &str, ids: &[u64]) -> Result<()>`; `&self` cannot mark threads `Sent` and cannot receive a reply |
| `src/preview/session.rs:406-416` | `pane send-text` + `pane send-keys enter` → `agent prompt` via the worker. `pane send-text` bypasses the `agent_blocked` pre-check: sending a note batch into a Claude pane sitting at an approval dialog **answers the dialog with note text**. |
| `src/preview/session.rs:299` | `Answer::Dead \| None => {}` silently drops a failed send; with stateful threads that strands a thread in `Sent` — record `ThreadState::Failed` + a `System` turn |

---

## 4. New modules

### `src/thread.rs` — §1 above.

### `src/agentio.rs` — the herdr agent surface

```rust
use std::ffi::OsStr;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatus { Idle, Working, Blocked, Done, #[default] Unknown }
impl AgentStatus {
    /// Four literal herdr spellings; anything else is Unknown, which is
    /// NON-resting on purpose — a state herdr adds must never look idle.
    pub fn from_wire(s: &str) -> AgentStatus;
    pub fn is_resting(self) -> bool; // Idle | Done ONLY
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef { pub agent: String, pub kind: String, pub value: String }

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
    pub offset: u64,
    pub prompt: String,
}

#[derive(Debug)]
pub enum PromptError {
    /// Rejected before any input was sent — the prompt was NOT delivered.
    Blocked,
    /// Accepted, but no working/blocked observed within herdr's 5000 ms gate.
    Stalled,
    /// Our --timeout expired. Delivery is UNPROVEN, not disproven.
    Timeout,
    Cli(String),
}

pub fn run_text(bin: &OsStr, args: &[&str]) -> Result<String, String>;
pub fn agent_get(bin: &OsStr, pane: &str) -> Result<AgentInfo, String>;

/// `kind == "path"` → the value. `kind == "id"` + claude → first glob hit of
/// `~/.claude/projects/*/<value>.jsonl`. None when unresolvable.
pub fn transcript_path(s: &SessionRef) -> Option<PathBuf>;

/// Pre-send gate + watermark. Errors when status is Working (never prompt a
/// working agent: --wait would match the PREVIOUS turn's completion) or
/// Blocked. Captures seq0 and the transcript byte offset BEFORE sending.
pub fn begin_turn(bin: &OsStr, pane: &str, prompt: &str) -> Result<Handle, PromptError>;

/// `agent prompt <pane> <text> --wait --until idle --until done --timeout N`.
/// Blocking — call from the reply worker thread only.
pub fn prompt_wait(bin: &OsStr, h: &Handle, timeout_ms: u64) -> Result<AgentInfo, PromptError>;

/// Primary capture: read the JSONL from h.offset, anchor on the first
/// non-sidechain `user` record, concatenate every following non-sidechain
/// `assistant` record's `message.content[].text`, stop at the next genuine
/// user prompt or EOF. Skips unparseable trailing lines (partial writes).
pub fn harvest_transcript(h: &Handle) -> Option<String>;

/// Fallback only: `agent read <pane> --source detection --lines 200`.
/// NEVER `--source recent` with --lines > viewport, and NEVER `--ansi`.
pub fn harvest_screen(bin: &OsStr, h: &Handle) -> Option<String>;
```

### `src/preview/reply.rs` — the off-UI-thread turn runner

```rust
use std::ffi::OsString;
use std::sync::mpsc::Sender;

pub struct ReplyJob {
    pub thread: u64,
    pub pane: String,
    pub prompt: String,
    pub timeout_ms: u64, // default 600_000
}

#[derive(Debug, Clone)]
pub enum ReplyMsg {
    Sent    { thread: u64, agent: String },
    Replied { thread: u64, text: String },
    /// `delivered` distinguishes Blocked (not sent) from Timeout/Stalled
    /// (unknown). Never auto-resend when `delivered != Some(false)`.
    Failed  { thread: u64, why: String, delivered: Option<bool> },
}

/// One serial worker thread. Never blocks the preview event loop (which
/// redraws every 100 ms and polls popup liveness).
pub fn spawn_reply_worker(tx: Sender<super::session::Event>, bin: OsString) -> Sender<ReplyJob>;
```

### `src/preview/card.rs` additions

```rust
pub enum CardKind {
    Thread(u64),
    /// Standalone composer (a brand-new thread).
    Composer,
    /// A thread card with the composer nested as its last rows;
    /// `(offset_in_card, height)` of the reply box.
    ThreadComposing { id: u64, composer: (usize, usize) },
}
pub struct Card { pub anchor: usize, pub lines: Vec<Line<'static>>, pub kind: CardKind }

/// Returns the rendered lines plus the nested composer's (offset, height).
/// Build the lines ONCE and measure `lines.len()` — never re-evaluate
/// collapse state to compute a height, or `shift` and `card_lines` disagree.
pub fn thread_card(
    thread: &crate::thread::Thread,
    lost: bool,
    collapsed: bool,
    composer: Option<(&crate::textarea::TextArea, usize)>, // (input, turn_index)
    width: usize,
    theme: crate::config::Theme,
) -> (Vec<Line<'static>>, Option<(usize, usize)>);

fn card_box_titled(
    title: Vec<Span<'static>>,
    rows: Vec<Vec<Span<'static>>>,
    width: usize,
    theme: crate::config::Theme,
    accent: bool,
) -> Vec<Line<'static>>;
```

Plus on `PreviewApp`: `collapsed: HashSet<u64>` (keyed by **thread id**, never by rank or doc line — rank-keying is the exact bug behind the `card_starts` regression at tests/preview.rs:862) and `focused_thread: Option<u64>` (the preview has no "thread under the cursor" because `snap_off_card` at app.rs:675 forbids it, so `Reply`/`ToggleThread`/`Resolve` need an explicit focus concept or must be driven from the list's `selected_note()`).

---

## 5. Highest risk, and the first thing to build

**Reply capture (item 4). Nothing else in the plan matters if it does not work, and it is the only part whose feasibility is not under your control.** Three independent ways it fails silently:

1. Claude Code runs on the **alternate screen with zero host scrollback** (`max_offset_from_bottom: 0`). Any reply longer than ~50 usable rows is unrecoverable by screen reading, and `truncated: false` will still be reported. There is no field anywhere that tells you a reply was cut off.
2. `agent prompt --wait` **does not track turns**, and `agent wait` is **level-triggered** (verified: `agent wait w1:p1 --until working` matched an already-current `working` state instantly). Prompt a mid-turn agent and you attribute the *previous* turn's text to your thread.
3. The transcript JSONL — the only clean channel — is an undocumented Claude-internal format, reached via a *derived* path (herdr surfaces `kind:"id"`, so you glob `~/.claude/projects/*/<uuid>.jsonl`), containing `isSidechain` subagent records that will splice a background agent's output into the user's review thread.

**De-risk it on day one, before writing a line of `thread.rs`:** add a throwaway `probe-reply` arm to `src/main.rs:5` (mirroring the existing `ask` / `pick-agent` dispatch) that runs the whole cycle against a live agent pane and prints both extractions side by side:

```
gitview probe-reply <pane> "<text>"
  → agent get: status, state_change_seq=seq0, agent_session{kind,value}
  → resolve transcript, record off0 = metadata(T).len()
  → agent prompt <pane> "<text>" --wait --until idle --until done --timeout 600000
  → assert seqN > seq0        (else: --wait matched a stale state)
  → print harvest_transcript() and harvest_screen() and their byte lengths
```

Run it ≥10 times covering: a one-line reply; a 200-line reply; a tool-heavy turn; a turn that hits a permission prompt mid-way; a question-only turn; a `Task`-spawning turn (sidechain filter); and a prompt fired at an already-working agent (must be *refused* by the gate, not sent). Ship nothing else until the transcript path returns the exact reply ≥9/10 and the screen fallback visibly degrades rather than lying.

If it fails, you learn on day 1 that items 3+4 collapse to a "sent / delivered / unknown" status badge with no reply text — which is a perfectly shippable feature, and radically cheaper than discovering it after Phases A–C are merged.

---

## 6. What in the maps contradicts the 7-item plan

**Item 5 (port reviewr's `turn.rs`) is the wrong tool and should be dropped, not ported.** This is the bluntest contradiction in the set. The reviewr tracker exists to *infer* turn boundaries for turns it did not initiate, by polling `herdr agent list` every 2 s and folding N agents' statuses per worktree. gitview initiates its own turns — it knows the start exactly, because it submitted it. `begin_turn` + `prompt --wait` + a `state_change_seq` delta gives you the boundary directly and *correctly*, with no polling, no membership resolution, no `git add -A && write-tree` every two seconds, and no `refs/worktree/` baseline. The reviewr map itself says so (its own §D.8: "*This is the single biggest simplification available*"). Worse, the tracker's entire promotion mechanism is about computing a **turn changeset baseline** — a diff-scoping concern gitview does not have, because gitview threads are anchored to a file range, not to a turn's tree. And it carries known defects you would be importing wholesale: rapid turns inside one poll gap are missed entirely; promotion is structurally one poll late; the divergence check is not gated on worktree state, so a *human* editing a file promotes a stale candidate; an unpromoted candidate makes every subsequent poll run `write-tree` forever. **Keep exactly two things**: `Status::from_wire` and `is_resting` (≈20 lines, already specified as `agentio::AgentStatus`) as the pre-send idle gate. Everything else — `TurnTracker`, `WorktreeState::fold`, `TurnHost`, `snapshot_worktree`, `TempIndex`, `TURN_BASE_REF`, `classify`/membership — is dead weight here. Revisit only if you later want to capture turns the *user* starts by typing into the agent pane.

**Item 3 will not work as a drop-in replacement for `deliver_notes`.** Three hard constraints the plan does not account for: (a) `deliver_notes` takes `&self` and blocks on a subprocess; `--wait --timeout 600000` inside the event loop freezes the preview pane for up to 10 minutes — it must move to a worker thread with a new `Event` variant (the diff worker is currently the only worker). (b) `--wait` returns a *state match*, not a turn completion; without the pre-send `agent get` idle gate plus the `state_change_seq` delta check it will happily hand you the previous turn's text. (c) `agent prompt` **rejects** when the agent is blocked, whereas today's `pane send-text` types into the dialog — that is a behaviour change (a correct one, but the picker needs to show status and the send path needs a "queued until idle" state, which does not exist in the plan).

**Item 2 is a one-line deletion with six downstream consequences.** Removing `clear_notes()` at `preview/session.rs:292` without also filtering `deliver_notes` by unsent turns makes every subsequent `p` re-send the entire answered review. It also removes the list's only auto-exit from `Mode::Notes` (`list/session.rs:299-305`) and falsifies four "is it empty" gates that currently mean *"nothing to send"* (`preview/app.rs:619`, `preview/session.rs:189`, `list/app.rs:337`, `list/app.rs:553`) and three counts (`preview/ui.rs:182`, `preview/session.rs:262`, `list/ui.rs:147`, `list/rows.rs:143`). Items 2 and 1 cannot be separate commits.

**Item 7's "nested card" collides with a hard invariant, not just a layout.** `keep_cursor_visible` (`app.rs:544-562`) clamps the cursor into the viewport in ROW space, then `snap_off_card` pushes it back out in LINE space — a card taller than `viewport_h` leaves the cursor off-screen and the pane shows no cursor at all. Today's 3-line cards make this unreachable; an expanded thread makes it routine. Collapsing mitigates but does not fix it. Related: `move_cursor` skips a whole card in one keypress, so one `j` over a 40-line thread yanks the viewport past the entire conversation. Budget for card-internal scroll stops, not just a taller box.

**Item 1: `NoteMeta` must NOT carry `Vec<Turn>`.** `ToList::Notes` is re-sent *in full on every `notes_rev` bump*; once an agent appends turns asynchronously that is O(entire review transcript) per reply on a socket the list drains on a 40 ms debounce. The list has no surface that could render a turn (`note_row` draws a fixed 2 lines, `ListRow::height()` is a hardcoded constant `row_at` depends on). Ship the projection (`turns` count + `state` + newest-turn `text`); if you later want inline expansion, add a pull command `ToPreview::RequestThread { id }` → `ToList::Thread { id, turns }`.

**Item 6: three landmines, in order of severity.** (a) `next_note_id` starts at 1 per process (`app.rs:175`) — a persisted store must reseed to `max(id)+1` or the first new thread collides with a restored one, and since every list→preview command is by id, a collision deletes or rewrites the wrong thread with no visible symptom. (b) A repo can host multiple preview processes (`orchestrate.rs:638 all_states` — one tab view plus one sidebar per host tab), so a single `{repo_hash}.json` gets concurrent writers; §2 sidesteps this by keying on the socket stem. (c) `begin_annotate`'s `new.or(old)` (`app.rs:795`) writes an OLD-side line number into a NEW-side field for deletion-only selections — currently a transient annoyance, permanently wrong once persisted.

**The premise itself has a gap nobody listed: "ask the agent to fix something and see the reply in the thread" is self-defeating with today's re-anchoring.** Re-anchoring is `DiffDoc::line_for_new` (`render.rs:79`), a bare line-number scan. When the agent *succeeds* and rewrites the file, the line number moves or disappears and `card::anchor_of` renders the thread at the top labelled "· anchor lost" — exactly for the threads that worked. Additionally, a committed change makes the worktree diff empty, `set_diff` (`app.rs:247-255`) takes its early return, and **no card is injected at all**: the thread still exists and still shows in the notes view, but the pane reads as blank. (It *does* still render in Branch scope, because `sync_doc:309` filters on `n.file` only — not scope, not commit — with line numbers computed against a different baseline.) The snippet-based re-anchor ladder in `Anchor` (`blob` → exact line → snippet match within ±N → lost) is not optional polish; without it the feature's happy path looks broken.

**Suggested build order:** probe (§5) → A (1+6) → B (2) → C (7) → D (3+4). Drop 5.