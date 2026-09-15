//! Persistent threaded review conversations, anchored to a code change.
//!
//! The preview pane is the sole owner and the sole id allocator; the list
//! mirrors a projection (`ipc::NoteMeta`) and acts strictly by id.
//!
//! Two design constraints worth stating up front, because both were learned
//! the expensive way:
//!
//! * A thread outlives the turn that answered it, so it cannot be keyed on a
//!   pane id — `pane move` reassigns those, and an agent name is cleared when
//!   its occupant exits. Threads key on `agent_session.value`.
//! * A thread breaks precisely when the agent *succeeds*: the fix rewrites the
//!   file, the recorded line numbers move, and a naive line-number re-anchor
//!   renders every successful thread as "anchor lost". Hence `Anchor::blob`
//!   and `Anchor::snippet`, and the ladder in `reanchor`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STORE_VERSION: u32 = 1;

/// Unix epoch seconds. Integer on purpose: every type below derives `Eq`, and
/// the IPC wire type forbids floats anyway.
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
    /// Delivery or capture failed. The text may or may not have reached the
    /// agent — never auto-resend from this state.
    Failed,
    /// User closed it. Never re-sent, hidden by default in the notes view.
    Resolved,
}

impl ThreadState {
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
    /// Set once this turn's text has been included in a delivery. The only
    /// thing preventing a second send from re-delivering the whole review.
    #[serde(default)]
    pub sent: bool,
}

/// Where a thread hangs. `start`/`end` are NEW-side line numbers; `0-0` is a
/// whole-file thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub file: PathBuf,
    pub start: u32,
    pub end: u32,
    /// The side the thread was written against, so re-showing picks the same
    /// diff.
    pub cached: bool,
    /// Selected diff lines, `-`/`+`/space prefixed. The only rewrite-resistant
    /// signal we have.
    #[serde(default)]
    pub snippet: String,
    /// `git hash-object` of the new side at creation.
    #[serde(default)]
    pub blob: Option<String>,
    /// HEAD when the thread was created — the "as-of" stamp.
    #[serde(default)]
    pub head_sha: Option<String>,
}

/// How confidently a thread still points at real code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorFix {
    /// The file is byte-identical to when the thread was written.
    Exact,
    /// The file changed but the snippet was found, at this new start line.
    Moved(u32),
    /// The file changed and the snippet is gone.
    Lost,
}

/// The agent a thread is in conversation with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub pane: String,
    pub agent: String,
    /// `agent_session.value` — the stable identity. `pane` is a hint only.
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
    /// Never empty for a persisted thread: `turns[0]` is the opening Human turn.
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
    pub fn new(id: u64, anchor: Anchor, text: String) -> Thread {
        let at = now_epoch();
        Thread {
            id,
            anchor,
            turns: vec![Turn {
                author: Author::Human,
                text,
                at,
                sent: false,
            }],
            state: ThreadState::Draft,
            agent: None,
            created_at: at,
            updated_at: at,
        }
    }

    pub fn last(&self) -> Option<&Turn> {
        self.turns.last()
    }

    /// The list-row preview line: the newest turn.
    /// One-line stand-in for the thread, used by the notes view.
    ///
    /// Your newest note, not the newest turn: an agent's reply is read in its
    /// own pane, and a notes list showing the agent's words back to you says
    /// nothing about what *you* asked. Falls back to the last turn only for
    /// the degenerate case of a thread with no Human turn at all.
    pub fn preview(&self) -> &str {
        self.turns
            .iter()
            .rev()
            .find(|t| t.author == Author::Human)
            .or_else(|| self.turns.last())
            .map(|t| t.text.as_str())
            .unwrap_or("")
    }

    pub fn has_unsent(&self) -> bool {
        self.state != ThreadState::Resolved
            && self
                .turns
                .iter()
                .any(|t| t.author == Author::Human && !t.sent)
    }

    pub fn unsent(&self) -> impl Iterator<Item = &Turn> {
        self.turns
            .iter()
            .filter(|t| t.author == Author::Human && !t.sent)
    }

    /// Positions of the unsent Human turns, so a delivery can mark back
    /// exactly what it asked about rather than every Human turn.
    pub fn unsent_indices(&self) -> Vec<usize> {
        self.turns
            .iter()
            .enumerate()
            .filter(|(_, t)| t.author == Author::Human && !t.sent)
            .map(|(i, _)| i)
            .collect()
    }

    /// The last Human turn that may still be rewritten in place.
    pub fn editable_turn(&self) -> Option<usize> {
        self.turns
            .iter()
            .rposition(|t| t.author == Author::Human && !t.sent)
    }

    pub fn push(&mut self, author: Author, text: String) {
        self.turns.push(Turn {
            author,
            text,
            at: now_epoch(),
            sent: false,
        });
        match author {
            Author::Agent => self.state = ThreadState::Answered,
            // A follow-up question puts the ball back in the user's court.
            // Leaving it `Answered` would render a "replied" badge on a
            // thread the footer is simultaneously counting as unsent.
            // `Resolved` and `Failed` are deliberately terminal.
            Author::Human
                if matches!(
                    self.state,
                    ThreadState::Sent | ThreadState::Answered | ThreadState::Failed
                ) =>
            {
                self.state = ThreadState::Draft;
            }
            _ => {}
        }
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

/// Where a thread's anchor now points, given the file's current content.
///
/// The ladder is blob → exact → snippet → lost. It exists because the happy
/// path *is* the breaking path: when the agent does what the thread asked,
/// the file changes and the recorded line numbers stop meaning anything.
pub fn reanchor(anchor: &Anchor, current_blob: Option<&str>, file_lines: &[&str]) -> AnchorFix {
    // Unchanged file: the recorded lines are still authoritative.
    if let (Some(want), Some(have)) = (anchor.blob.as_deref(), current_blob)
        && want == have
    {
        return AnchorFix::Exact;
    }
    // Whole-file threads have nothing to move.
    if anchor.end == 0 {
        return AnchorFix::Exact;
    }
    // Find the snippet's added/context lines verbatim in the new file.
    let needle: Vec<&str> = anchor
        .snippet
        .lines()
        .filter(|l| !l.starts_with('-'))
        .map(|l| l.get(1..).unwrap_or("").trim_end())
        .filter(|l| !l.trim().is_empty())
        .collect();
    if needle.is_empty() {
        return AnchorFix::Lost;
    }
    let trimmed: Vec<&str> = file_lines.iter().map(|l| l.trim_end()).collect();
    for (i, window) in trimmed.windows(needle.len()).enumerate() {
        if window == needle.as_slice() {
            return AnchorFix::Moved(i as u32 + 1);
        }
    }
    AnchorFix::Lost
}

// ---- store ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadStore {
    pub version: u32,
    /// Resolved `git rev-parse --show-toplevel`, for diagnosing a key clash.
    pub repo: PathBuf,
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
    /// message so the caller can flash it, and quarantines the bad file
    /// rather than overwriting it. Discarding a review is much worse than
    /// discarding a pane layout, so this does not `.ok()` anything away.
    pub fn load(path: &Path, repo: &Path) -> (ThreadStore, Option<String>) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            // Missing is the normal first-run case, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (ThreadStore::empty(repo), None);
            }
            // Unreadable for any other reason (EACCES, EIO, a stray
            // directory). The bytes may still be a perfectly good review, so
            // move it aside like the two branches below rather than letting
            // the next `save` overwrite it with this empty store.
            Err(e) => {
                return (
                    ThreadStore::empty(repo),
                    Some(quarantine(path, format!("could not read threads: {e}"))),
                );
            }
        };
        match serde_json::from_slice::<ThreadStore>(&bytes) {
            Ok(mut store) if store.version == STORE_VERSION => {
                let err = store.reseed();
                (store, err)
            }
            Ok(store) => {
                let msg = format!(
                    "threads file is version {} (expected {}); quarantined",
                    store.version, STORE_VERSION
                );
                (ThreadStore::empty(repo), Some(quarantine(path, msg)))
            }
            Err(e) => (
                ThreadStore::empty(repo),
                Some(quarantine(path, format!("threads file unreadable: {e}"))),
            ),
        }
    }

    /// Restore the id watermark and drop structurally impossible rows. A
    /// persisted `next_id` that is not above every id on disk is the one
    /// corruption that causes silent cross-thread writes.
    fn reseed(&mut self) -> Option<String> {
        let before = self.threads.len();
        self.threads.retain(|t| !t.turns.is_empty());
        let dropped = before - self.threads.len();

        let mut seen = std::collections::HashSet::new();
        self.threads.retain(|t| seen.insert(t.id));
        let deduped = before - dropped - self.threads.len();

        let max = self.threads.iter().map(|t| t.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max + 1);

        match (dropped, deduped) {
            (0, 0) => None,
            (d, 0) => Some(format!("dropped {d} empty thread(s) on load")),
            (0, u) => Some(format!("dropped {u} duplicate thread id(s) on load")),
            (d, u) => Some(format!("dropped {d} empty and {u} duplicate thread(s)")),
        }
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

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.threads.len();
        self.threads.retain(|t| t.id != id);
        self.threads.len() != before
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    pub fn len(&self) -> usize {
        self.threads.len()
    }

    /// Anything still worth delivering.
    pub fn has_unsent(&self) -> bool {
        self.threads.iter().any(Thread::has_unsent)
    }

    /// Write via a temp sibling + rename, so an interrupted save cannot leave
    /// a half-written review behind.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        // Per-process staging: two preview processes can legitimately share a
        // store path, and a shared tmp name lets one truncate the other's
        // half-written document and then rename the wreckage into place.
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Move a bad store aside so the next save does not overwrite it, and return
/// the message to flash.
fn quarantine(path: &Path, msg: String) -> String {
    let mut bad = path.with_extension("json.corrupt");
    // `rename` replaces the destination silently, so a second quarantine
    // would destroy the review the first one was saving.
    if bad.exists() {
        bad = path.with_extension(format!("json.corrupt.{}", now_epoch()));
    }
    match std::fs::rename(path, &bad) {
        Ok(()) => format!("{msg} → {}", bad.display()),
        Err(e) => format!("{msg} (could not quarantine: {e})"),
    }
}

/// `$HERDR_PLUGIN_STATE_DIR/threads/<view-key>.json`.
///
/// The key is the IPC socket stem, because a single repo can host several
/// preview processes at once (one tab view plus one sidebar per host tab) and
/// they would otherwise be concurrent writers to one file. Standalone runs
/// have no socket, so they fall back to hashing the checkout path.
pub fn store_path(socket: Option<&Path>, repo: &Path) -> PathBuf {
    let key = socket
        .and_then(Path::file_stem)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::orchestrate::repo_hash(repo));
    crate::logx::state_dir()
        .join("threads")
        .join(format!("{key}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> Anchor {
        Anchor {
            file: PathBuf::from("src/a.rs"),
            start: 10,
            end: 12,
            cached: false,
            snippet: String::new(),
            blob: None,
            head_sha: None,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitview-thread-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_reply_moves_the_thread_to_answered() {
        let mut t = Thread::new(1, anchor(), "why?".into());
        assert_eq!(t.state, ThreadState::Draft);
        assert!(t.has_unsent());

        t.mark_sent(AgentRef {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            session: Some("uuid".into()),
            session_kind: Some("id".into()),
        });
        assert_eq!(t.state, ThreadState::Sent);
        assert!(!t.has_unsent(), "a sent turn must not be re-delivered");

        t.push(Author::Agent, "because.".into());
        assert_eq!(t.state, ThreadState::Answered);
        assert!(!t.has_unsent(), "an agent turn is not something we send");
        // The reply is stored, but `preview` is your own newest note — the
        // notes view lists what you asked, not what came back.
        assert_eq!(t.turns.last().unwrap().text, "because.");
        assert_eq!(t.preview(), "why?");
    }

    #[test]
    fn a_reply_after_an_answer_is_unsent_again() {
        let mut t = Thread::new(1, anchor(), "why?".into());
        t.mark_sent(AgentRef {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            session: None,
            session_kind: None,
        });
        t.push(Author::Agent, "because.".into());
        assert_eq!(t.state, ThreadState::Answered);
        t.push(Author::Human, "follow-up".into());
        assert!(t.has_unsent());
        assert_eq!(t.unsent().count(), 1, "only the new turn is pending");
        assert_eq!(
            t.state,
            ThreadState::Draft,
            "the badge must not still claim the agent has the ball while \
             the footer counts this thread as unsent"
        );
    }

    #[test]
    fn resolved_threads_are_never_resent() {
        let mut t = Thread::new(1, anchor(), "why?".into());
        t.state = ThreadState::Resolved;
        assert!(!t.has_unsent());
    }

    #[test]
    fn next_id_is_reseeded_above_every_loaded_id() {
        // The dangerous corruption: next_id below an existing id.
        let dir = tmpdir("reseed");
        let path = dir.join("t.json");
        let mut store = ThreadStore::empty(Path::new("/repo"));
        store.threads.push(Thread::new(7, anchor(), "a".into()));
        store.threads.push(Thread::new(9, anchor(), "b".into()));
        store.next_id = 2;
        store.save(&path).unwrap();

        let (mut loaded, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert_eq!(err, None);
        assert_eq!(loaded.next_id, 10);
        assert_eq!(loaded.alloc_id(), 10, "a new thread must not collide");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_are_dropped_on_load() {
        let dir = tmpdir("dupe");
        let path = dir.join("t.json");
        let mut store = ThreadStore::empty(Path::new("/repo"));
        store.threads.push(Thread::new(3, anchor(), "first".into()));
        store
            .threads
            .push(Thread::new(3, anchor(), "shadow".into()));
        store.save(&path).unwrap();

        let (loaded, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert_eq!(loaded.threads.len(), 1);
        assert_eq!(loaded.threads[0].preview(), "first");
        assert!(err.unwrap().contains("duplicate"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let (store, err) = ThreadStore::load(Path::new("/nope/absent.json"), Path::new("/repo"));
        assert!(store.is_empty());
        assert_eq!(err, None, "first run must not flash");
    }

    #[test]
    fn a_corrupt_store_is_quarantined_not_overwritten() {
        let dir = tmpdir("corrupt");
        let path = dir.join("t.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let (store, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert!(store.is_empty());
        assert!(err.unwrap().contains("unreadable"));
        assert!(!path.exists(), "the bad file must be moved aside");
        assert!(
            dir.join("t.json.corrupt").exists(),
            "and kept, not deleted — it is someone's review"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A read failure that is not "missing" used to fall through to an empty
    /// store, which the next `persist` then wrote over the top of a review
    /// that was never unreadable in the first place — only unreachable.
    #[test]
    fn an_unreadable_store_is_moved_aside_rather_than_overwritten() {
        let dir = tmpdir("unreadable");
        let path = dir.join("t.json");
        // A directory at the store path fails `read` with EISDIR, not
        // NotFound — portable, unlike relying on chmod as a non-root user.
        std::fs::create_dir(&path).unwrap();

        let (store, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert!(store.is_empty());
        let err = err.unwrap();
        assert!(err.contains("could not read threads"), "{err}");
        assert!(
            dir.join("t.json.corrupt").exists(),
            "the unreadable entry must be moved aside so the next save cannot clobber it"
        );
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `rename` replaces its destination, so quarantining twice used to
    /// destroy whatever the first quarantine was preserving.
    #[test]
    fn a_second_quarantine_does_not_destroy_the_first() {
        let dir = tmpdir("twice");
        let path = dir.join("t.json");

        std::fs::write(&path, b"first review").unwrap();
        let (_, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert!(err.is_some());
        assert_eq!(
            std::fs::read(dir.join("t.json.corrupt")).unwrap(),
            b"first review"
        );

        std::fs::write(&path, b"second review").unwrap();
        let (_, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert!(err.is_some());
        assert_eq!(
            std::fs::read(dir.join("t.json.corrupt")).unwrap(),
            b"first review",
            "the original quarantine must survive"
        );
        let saved: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.contains(".corrupt"))
            .collect();
        assert_eq!(saved.len(), 2, "both reviews kept: {saved:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_future_version_is_quarantined_not_parsed() {
        let dir = tmpdir("version");
        let path = dir.join("t.json");
        std::fs::write(
            &path,
            br#"{"version":99,"repo":"/r","next_id":1,"threads":[]}"#,
        )
        .unwrap();

        let (store, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert!(store.is_empty());
        assert!(err.unwrap().contains("version 99"));
        assert!(dir.join("t.json.corrupt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_round_trips_every_field() {
        let dir = tmpdir("round");
        let path = dir.join("t.json");
        let mut store = ThreadStore::empty(Path::new("/repo"));
        store.base_ref = Some("origin/main".into());
        let mut t = Thread::new(1, anchor(), "q".into());
        t.anchor.snippet = "+added\n context\n".into();
        t.anchor.blob = Some("deadbeef".into());
        t.mark_sent(AgentRef {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            session: Some("uuid".into()),
            session_kind: Some("id".into()),
        });
        t.push(Author::Agent, "a".into());
        store.threads.push(t.clone());
        store.save(&path).unwrap();

        let (loaded, err) = ThreadStore::load(&path, Path::new("/repo"));
        assert_eq!(err, None);
        assert_eq!(loaded.base_ref.as_deref(), Some("origin/main"));
        assert_eq!(loaded.threads, vec![t]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tmpdir("tmp");
        let path = dir.join("t.json");
        ThreadStore::empty(Path::new("/repo")).save(&path).unwrap();
        assert!(path.exists());
        // Match on the suffix, not one fixed name: the staging file carries a
        // pid, so asserting against `t.json.tmp` would pass vacuously.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging files left behind: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- re-anchoring -----------------------------------------------------

    #[test]
    fn an_unchanged_file_anchors_exactly() {
        let mut a = anchor();
        a.blob = Some("abc".into());
        assert_eq!(reanchor(&a, Some("abc"), &[]), AnchorFix::Exact);
    }

    #[test]
    fn a_rewritten_file_follows_the_snippet() {
        // The happy path that breaks naive re-anchoring: the agent did the
        // fix, so the code moved.
        let mut a = anchor();
        a.blob = Some("old".into());
        a.snippet = "+    let x = compute();\n+    return x;\n".into();
        let file = vec![
            "// a new header comment",
            "// pushed everything down",
            "fn main() {",
            "    let x = compute();",
            "    return x;",
            "}",
        ];
        assert_eq!(reanchor(&a, Some("new"), &file), AnchorFix::Moved(4));
    }

    #[test]
    fn a_deleted_snippet_is_lost_not_silently_exact() {
        let mut a = anchor();
        a.blob = Some("old".into());
        a.snippet = "+    let x = compute();\n".into();
        assert_eq!(
            reanchor(&a, Some("new"), &["fn main() {}"]),
            AnchorFix::Lost
        );
    }

    #[test]
    fn removed_lines_do_not_participate_in_the_search() {
        let mut a = anchor();
        a.blob = Some("old".into());
        // Only the `-` line would match; it must not count.
        a.snippet = "-    let gone = 1;\n".into();
        assert_eq!(
            reanchor(&a, Some("new"), &["    let gone = 1;"]),
            AnchorFix::Lost
        );
    }

    #[test]
    fn a_whole_file_thread_never_goes_lost() {
        let mut a = anchor();
        a.start = 0;
        a.end = 0;
        a.blob = Some("old".into());
        assert_eq!(reanchor(&a, Some("new"), &["anything"]), AnchorFix::Exact);
    }
}
