//! Preview-pane state: the currently shown diff, scroll position, and the
//! small state machine (splash / diff / empty / error).
//!
//! Like the list's `App`, this owns no channels or terminals. `preview::run`
//! feeds it IPC messages, key events, and diff-worker results, then renders it
//! via `preview::ui`. That keeps the diff parsing/scroll math easy to test.

use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::config::Config;
use crate::git::{ChangeKind, Repo, Scope};
use crate::keymap::{Action, Keymap};
use crate::thread::{Anchor, Author, Thread, ThreadStore};

/// Hard cap on rendered diff lines; beyond this we show a truncation notice so
/// a 100k-line diff can't stall the render loop.
const MAX_LINES: usize = 20_000;

use super::card::{self, Card, MIN_WIDTH as MIN_CARD_WIDTH};
use super::compose::{Composer, Outcome};

/// The fields of a `ToPreview::Show`, kept together so we can compare the
/// request that produced a diff against the one currently selected (stale
/// results from the worker are dropped when they differ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowReq {
    pub file: PathBuf,
    pub orig_path: Option<PathBuf>,
    pub scope: Scope,
    pub cached: bool,
    pub kind: ChangeKind,
    /// History view: show this commit's change instead of a live diff.
    pub commit: Option<String>,
}

/// What the open composer is going to do when it commits. A composer is no
/// longer always "a new note": it can also rewrite an unsent turn or add a
/// reply to an existing conversation.
#[derive(Debug, Clone)]
pub enum Target {
    /// Start a new thread at `Pending::anchor`.
    New,
    /// Rewrite turn `turn` of thread `id` in place (only ever an unsent
    /// Human turn — a delivered one is history).
    Edit { id: u64, turn: usize },
    /// Append a new Human turn to thread `id`.
    Reply(u64),
}

/// The composer's subject: where it hangs and what it will do.
#[derive(Debug, Clone)]
pub struct Pending {
    pub anchor: Anchor,
    pub target: Target,
}

/// A popup the run loop should open on our behalf.
pub enum PopupReq {
    PickAgent,
}

pub enum State {
    /// Nothing to show yet: dim centered message.
    Splash(&'static str),
    /// A diff is loaded in `doc`.
    Diff,
    /// The file has no changes in the requested view.
    Empty,
    /// The file is binary; no diff to render.
    Binary,
    /// The diff build failed; holds the first stderr line.
    Error(String),
}

pub struct PreviewApp {
    pub cfg: Config,
    pub repo: Repo,
    pub keys: Keymap,

    /// Last Show request (what the header/stale-guard describe).
    pub current: Option<ShowReq>,
    /// The built diff (kept for click-to-unfold rebuilds).
    built: Option<super::render::DiffDoc>,
    /// Styled, capped diff text (plus a truncation notice line when capped).
    pub doc: Text<'static>,
    /// `doc`, word-wrapped to `viewport_w`, plus the row<->line maps. Kept in
    /// sync with `doc`/`viewport_w` via `sync_wrapped` — every scroll/click
    /// computation must go through this (never `doc.lines.len()` directly),
    /// since a wrapped line renders as more rows than it has logical lines.
    wrapped: super::render::WrappedDoc,
    /// First inserted line's new-file number (editor jump target).
    pub first_change: Option<u32>,
    /// Extra lines hidden by the cap (0 = not truncated).
    pub scroll: u16,
    /// Last body height, remembered so Page math is viewport-aware.
    pub viewport_h: u16,
    /// Body width of the last draw; note cards are boxed to it.
    pub viewport_w: u16,

    /// Branch-scope base ref, resolved once for the header.
    pub base: Option<String>,

    // ---- review notes / selection ----
    /// Cursor line in the rendered doc (drives selection).
    pub cursor_line: usize,
    /// Selection anchor (`v`); selection = anchor..=cursor.
    pub select_anchor: Option<usize>,
    /// Persistent review threads across files. The preview pane is the sole
    /// owner and the sole id allocator.
    pub store: ThreadStore,
    /// Where the loaded store lives, so every mutation can write it back.
    pub store_path: PathBuf,
    /// Set when the composer is open: what it will commit.
    pub pending: Option<Pending>,
    /// The open inline composer, if any. While set it owns every keystroke.
    pub composer: Option<Composer>,
    /// Popup for the run loop to open.
    pub popup_request: Option<PopupReq>,
    /// `n` pressed here: ask the list pane to open the notes view.
    pub notes_view_request: bool,
    /// Enter pressed here: ask the list pane to open the editor on the
    /// current selection (same flow as Enter in the list).
    pub edit_request: bool,
    /// Pristine copies of the lines currently tinted, so a cursor move can
    /// restore them instead of re-cloning the whole (up to 20k-line) doc.
    saved_tint: Vec<(usize, ratatui::text::Line<'static>)>,
    /// The file the current doc was built for (cursor-preservation check).
    shown_file: Option<PathBuf>,
    /// Rendered indices of injected note-card lines (excluded from ranges).
    /// Always ascending — cards are spliced in anchor order — so every lookup
    /// goes through `is_card_line`/`cards_before`, which binary-search it. A
    /// linear scan was free for three-line notes and is not for a card that
    /// can be a hundred lines of conversation.
    card_lines: Vec<usize>,
    /// Threads the user has collapsed to their newest turn (`z`).
    collapsed: std::collections::HashSet<u64>,
    /// Threads queued with the reply worker but not yet finished. Deliberately
    /// not persisted: a queue lives in a worker thread that dies with the
    /// pane, so restoring it would claim requests nobody is going to run.
    pub in_flight: std::collections::HashSet<u64>,
    /// The first doc line of each note's card, keyed by note id.
    /// `(thread id, first doc line, height)` for each card in the shown file.
    /// The height is recorded here rather than re-derived from `card_lines`:
    /// two cards on one anchor are spliced back-to-back, so their lines form
    /// a single unbroken run that no walk can tell apart.
    card_starts: Vec<(u64, usize, usize)>,
    /// Where the open composer box sits in the doc: `(first line, height)`.
    composer_span: Option<(usize, usize)>,
    /// Note id to scroll to once its file's diff arrives (notes view hover).
    pending_focus: Option<u64>,
    /// Bumped on every thread mutation; the run loop broadcasts on change.
    pub notes_rev: u64,
    /// Transient footer flash message.
    pub flash: Option<(String, std::time::Instant)>,

    pub state: State,
    pub should_quit: bool,
    /// True only when *this* pane initiated the quit (via `q`), so it should
    /// tear down the whole herdr view. A `Quit` message or EOF just exits.
    pub close_view: bool,
}

impl PreviewApp {
    pub fn new(cfg: Config, repo: Repo, keys: Keymap, store_path: PathBuf) -> PreviewApp {
        let (store, load_err) = ThreadStore::load(&store_path, &repo.root);
        // The loaded threads must reach the list, and `tick` only broadcasts
        // when `notes_rev` changed — so start at 1, not 0.
        let notes_rev = u64::from(!store.is_empty());
        PreviewApp {
            cfg,
            repo,
            keys,
            current: None,
            built: None,
            doc: Text::default(),
            wrapped: super::render::WrappedDoc::default(),
            first_change: None,
            scroll: 0,
            viewport_h: 0,
            viewport_w: 0,
            base: None,
            cursor_line: 0,
            select_anchor: None,
            store,
            store_path,
            pending: None,
            composer: None,
            popup_request: None,
            notes_view_request: false,
            edit_request: false,
            saved_tint: Vec::new(),
            shown_file: None,
            card_lines: Vec::new(),
            collapsed: std::collections::HashSet::new(),
            in_flight: std::collections::HashSet::new(),
            card_starts: Vec::new(),
            composer_span: None,
            pending_focus: None,
            notes_rev,
            // A load failure is never fatal, but it must be visible: the
            // alternative is a user whose review silently did not come back.
            flash: load_err.map(|e| (e, std::time::Instant::now())),
            state: State::Splash("waiting for file list…"),
            should_quit: false,
            close_view: false,
        }
    }

    /// Every thread in the store, in creation order.
    pub fn threads(&self) -> &[Thread] {
        &self.store.threads
    }

    /// Write the store back. Persistence failures flash rather than panic —
    /// losing the on-disk copy is bad, losing the in-memory review is worse.
    fn persist(&mut self) {
        if let Err(e) = self.store.save(&self.store_path) {
            crate::logx::log(format!("thread store save failed: {e}"));
            self.flash(format!("could not save threads: {e}"));
        }
    }

    /// The single place a thread mutation is finalised: bump the revision so
    /// the list is told, write to disk, and re-splice the cards.
    fn threads_changed(&mut self) {
        self.notes_rev += 1;
        self.persist();
        self.rebuild();
    }

    /// The list connected; if we haven't shown anything yet, switch the splash
    /// text from "waiting" to "no file selected".
    pub fn on_connected(&mut self) {
        if self.current.is_none() {
            self.state = State::Splash("no file selected");
        }
    }

    /// The list has nothing selected any more — drop the shown diff.
    pub fn clear(&mut self) {
        self.current = None;
        self.built = None;
        self.shown_file = None;
        self.saved_tint.clear();
        self.doc = Text::default();
        self.sync_wrapped();
        self.first_change = None;
        self.scroll = 0;
        self.state = State::Splash("no file selected");
    }

    // ---- Show / diff results ---------------------------------------------

    /// Record a new Show request. Scroll resets on a file change, is preserved
    /// on a same-file refresh (e.g. Tab staged toggle or auto-refresh). The
    /// old `doc` is kept until the worker returns, so there is no flicker.
    pub fn begin_show(&mut self, req: ShowReq) {
        let same_file = self
            .current
            .as_ref()
            .map(|c| c.file == req.file)
            .unwrap_or(false);
        if !same_file {
            self.scroll = 0;
        }
        if req.scope == Scope::Branch && self.base.is_none() {
            self.base = Some(self.repo.detect_base());
        }
        self.current = Some(req);
    }

    /// Apply a worker result, dropping it if it is stale (the selection moved
    /// on while the diff was computing).
    pub fn apply_diff(&mut self, req: &ShowReq, result: Result<super::render::DiffDoc, String>) {
        if self.current.as_ref() != Some(req) {
            return; // stale — a newer Show already superseded this one
        }
        match result {
            Ok(doc) => self.set_diff(doc),
            Err(msg) => self.state = State::Error(msg),
        }
    }

    fn set_diff(&mut self, built: super::render::DiffDoc) {
        self.first_change = built.first_change;
        if built.binary {
            self.built = None;
            self.saved_tint.clear();
            self.doc = Text::default();
            self.sync_wrapped();
            self.state = State::Binary;
            self.clamp_scroll();
            return;
        }
        if built.is_empty {
            self.built = None;
            self.saved_tint.clear();
            self.doc = Text::default();
            self.sync_wrapped();
            self.state = State::Empty;
            self.clamp_scroll();
            return;
        }
        // A same-file refresh (auto-poll, stage toggle) keeps the cursor and
        // any live selection; only a *different* file resets them.
        let same_file = self
            .current
            .as_ref()
            .map(|req| self.shown_file.as_ref() == Some(&req.file))
            .unwrap_or(false);
        self.shown_file = self.current.as_ref().map(|req| req.file.clone());
        self.built = Some(built);
        if !same_file {
            self.cursor_line = 0;
            self.select_anchor = None;
        }
        self.state = State::Diff;
        self.clamp_scroll();
        self.rebuild();
        if let Some(id) = self.pending_focus.take() {
            self.scroll_to_note(id);
        }
    }

    /// Copy the built text into `doc`, applying the render cap and injecting
    /// the current file's note cards under their anchor lines.
    fn sync_doc(&mut self) {
        let Some(built) = &self.built else {
            return;
        };
        // Cap the *source* before anything is spliced in. Capping afterwards
        // would leave card_lines / card_starts / composer_span pointing at
        // lines that had just been truncated away — on a very large diff the
        // composer box vanished while still owning every keystroke.
        let mut doc = built.text.clone();
        let dropped = doc.lines.len().saturating_sub(MAX_LINES);
        if dropped > 0 {
            doc.lines.truncate(MAX_LINES);
        }

        // Note cards: a boxed block spliced in under the anchor line, so a
        // note reads as a comment on the code rather than another diff row.
        // Each card is 1+ lines, so the index bookkeeping below tracks every
        // line it occupies (`card_lines`) plus where each card starts
        // (`card_starts`, keyed by note id for `scroll_to_note`).
        self.card_lines.clear();
        self.card_starts.clear();
        self.composer_span = None;
        if let Some(req) = &self.current {
            let width = self.viewport_w.max(MIN_CARD_WIDTH) as usize;
            // While a note is being rewritten its own card is hidden — the
            // composer box standing in its place *is* that note.
            let editing = self.composer.as_ref().and_then(|c| c.editing);
            let mut cards: Vec<Card> = self
                .store
                .threads
                .iter()
                .filter(|t| t.anchor.file == req.file && Some(t.id) != editing)
                .map(|t| {
                    let (anchor, lost) = card::anchor_of(built, t.anchor.end);
                    // Built as spans, not one string: the state badge carries
                    // its own colour, and a narrow pane must drop the turn
                    // count before it drops "anchor lost".
                    let mut title = vec![Span::styled(
                        format!(
                            " {} ",
                            card::range_label("note", t.anchor.start, t.anchor.end)
                        ),
                        card::title_style(),
                    )];
                    if t.turns.len() > 1 {
                        title.push(Span::styled(
                            format!("· {} turns ", t.turns.len()),
                            card::dim_style(),
                        ));
                    }
                    // In flight beats the stored state: between queuing and
                    // delivery a thread is still `Draft` on disk, and showing
                    // "draft" for the minutes an agent takes reads as "your
                    // send did nothing".
                    if self.in_flight.contains(&t.id) {
                        title.push(Span::styled(
                            "· asking… ",
                            card::badge_style(crate::thread::ThreadState::Sent),
                        ));
                    } else if t.state != crate::thread::ThreadState::Draft {
                        title.push(Span::styled(
                            format!("· {} ", t.state.badge()),
                            card::badge_style(t.state),
                        ));
                    }
                    if lost {
                        // Its line is gone from this diff (the file changed
                        // under it). Say so rather than quietly rendering it
                        // at the top as if it were a whole-file note.
                        title.push(Span::styled("· anchor lost ", Style::new().fg(Color::Red)));
                    }
                    Card {
                        anchor,
                        lines: card::thread_card(
                            title,
                            t,
                            width,
                            self.cfg.theme,
                            self.collapsed.contains(&t.id),
                        ),
                        kind: card::CardKind::Thread(t.id),
                    }
                })
                .collect();
            // The composer goes in last, so among cards anchored to the same
            // line the box you are typing in sits closest to the code.
            let composing = self.composer.as_ref().map(|c| {
                let pending = self.pending.as_ref();
                let (anchor, _) =
                    card::anchor_of(built, pending.map(|p| p.anchor.end).unwrap_or(0));
                let prefix = match pending.map(|p| &p.target) {
                    Some(Target::Edit { .. }) => "edit note",
                    Some(Target::Reply(_)) => "reply",
                    _ => "new note",
                };
                let (start, end) = pending
                    .map(|p| (p.anchor.start, p.anchor.end))
                    .unwrap_or((0, 0));
                Card {
                    anchor,
                    lines: card::composer_card(
                        &card::range_label(prefix, start, end),
                        &c.input,
                        width,
                        self.cfg.theme,
                    ),
                    kind: card::CardKind::Composer,
                }
            });
            if let Some(card) = composing {
                cards.push(card);
            }

            // Accent the gutter of every commented line *before* anything is
            // spliced in, while the anchors still index the unshifted doc, so
            // a line with a note is recognizable once its card scrolls away.
            for card in &cards {
                if card.anchor > 0 {
                    card::accent_gutter(&mut doc.lines, card.anchor - 1, built);
                }
            }
            // An anchor past the end of the rendered doc (the render cap, or a
            // persisted thread that outlived the lines it was written against)
            // splices at the end. Clamp after the gutter pass, which needs the
            // true anchor to no-op, but before the bookkeeping below, so
            // `card_lines` records where the card actually landed.
            let cap = doc.lines.len();
            for card in &mut cards {
                card.anchor = card.anchor.min(cap);
            }
            // Ascending by anchor (stable, so notes on one line keep their
            // order), then spliced in from the bottom up so an insertion
            // never shifts an anchor that hasn't been used yet.
            cards.sort_by_key(|card| card.anchor);
            for card in cards.iter().rev() {
                let at = card.anchor.min(doc.lines.len());
                doc.lines.splice(at..at, card.lines.iter().cloned());
            }
            // Then top-down for the index math: each card sits after every
            // card already spliced in above it. `card_starts` is keyed by note
            // id, not by rank — a rank would be computed over a different set
            // than the one that produced it whenever a note is being edited.
            let mut shift = 0usize;
            for card in &cards {
                let start = card.anchor + shift;
                match card.kind {
                    card::CardKind::Thread(id) => {
                        self.card_starts.push((id, start, card.lines.len()))
                    }
                    card::CardKind::Composer => {
                        self.composer_span = Some((start, card.lines.len()))
                    }
                }
                self.card_lines.extend(start..start + card.lines.len());
                shift += card.lines.len();
            }
        }

        // The notice is chrome, like a card: it belongs to no source line, so
        // the cursor must not be able to rest on it and report a line number.
        if dropped > 0 {
            self.card_lines.push(doc.lines.len());
            doc.lines.push(Line::from(Span::styled(
                format!("… diff truncated ({dropped} more lines)"),
                Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            )));
        }
        self.doc = doc;
    }

    // ---- mouse ------------------------------------------------------------

    /// Wheel scrolls; a left click moves the cursor (and expands folds);
    /// dragging extends a selection. `y` is the terminal row (body starts at
    /// 1, under the header).
    pub fn on_mouse(&mut self, kind: crossterm::event::MouseEventKind, y: u16) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match kind {
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::Down(MouseButton::Left) if y >= 1 => {
                // `scroll`/`y` are rendered-row coordinates (what a wrapped
                // line actually paints); map back to the logical doc line
                // everything else here (cards, cursor, folds) addresses.
                let row = self.scroll as usize + (y - 1) as usize;
                let line = self.line_of_row(row);
                // Cards count as their neighbors; clicks on them do nothing.
                let card_free = !self.is_card_line(line);
                let to_built = self.doc_to_built(line);
                if let (Some(bl), Some(built)) = (to_built, self.built.as_mut())
                    && built.unfold_at(bl)
                {
                    self.clamp_scroll();
                    self.rebuild();
                    return;
                }
                if card_free && line < self.content_lines() {
                    self.select_anchor = None;
                    self.cursor_line = line;
                    self.restyle();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if y >= 1 => {
                let row = self.scroll as usize + (y - 1) as usize;
                let line = self
                    .line_of_row(row)
                    .min(self.content_lines().saturating_sub(1));
                if self.select_anchor.is_none() {
                    self.select_anchor = Some(self.cursor_line);
                }
                // Dragging across a card extends past it, never onto it.
                let dir = if line >= self.cursor_line { 1 } else { -1 };
                self.cursor_line = self.snap_off_card(line, dir);
                self.restyle();
            }
            _ => {}
        }
    }

    /// Is this doc line part of an injected card (and so not source)?
    /// `card_lines` is ascending by construction, so this is a binary search.
    fn is_card_line(&self, line: usize) -> bool {
        self.card_lines.binary_search(&line).is_ok()
    }

    /// How many injected card lines sit strictly above `line`.
    fn cards_before(&self, line: usize) -> usize {
        self.card_lines.partition_point(|c| *c < line)
    }

    /// Map a doc line index (with cards injected) back to the built index.
    fn doc_to_built(&self, line: usize) -> Option<usize> {
        if self.is_card_line(line) {
            return None;
        }
        Some(line - self.cards_before(line))
    }

    // ---- scrolling --------------------------------------------------------

    /// Record the body size of the current draw. A width change re-boxes the
    /// note cards, so they always match the pane they are drawn in.
    pub fn set_viewport(&mut self, w: u16, h: u16) {
        self.viewport_h = h;
        if self.viewport_w != w {
            self.viewport_w = w;
            if matches!(self.state, State::Diff) {
                self.rebuild(); // re-boxes cards and re-wraps for the new width
            } else {
                self.sync_wrapped(); // still re-wrap so `wrapped` stays current
            }
        }
        self.clamp_scroll();
    }

    /// Number of logical content lines in the document (incl. any notice).
    /// This is *not* what's on screen once a line wraps — use
    /// `visible_row_count` for anything measured against `scroll`.
    fn content_lines(&self) -> usize {
        self.doc.lines.len()
    }

    /// Re-wrap `doc` to `viewport_w`. Call after either changes, before any
    /// scroll-bound math (`max_scroll`, click mapping, ...) runs against it.
    fn sync_wrapped(&mut self) {
        self.wrapped = super::render::wrap_diff_text(&self.doc, self.viewport_w as usize);
    }

    /// The wrapped text actually painted in the diff body.
    pub fn wrapped_text(&self) -> Text<'static> {
        self.wrapped.text.clone()
    }

    /// Rendered rows currently in the document — the unit `scroll` and
    /// on-screen `y` coordinates count in, once lines can wrap.
    fn visible_row_count(&self) -> usize {
        self.wrapped.text.lines.len()
    }

    /// The rendered row logical `line` starts on.
    fn row_of_line(&self, line: usize) -> usize {
        self.wrapped.line_to_row.get(line).copied().unwrap_or(0)
    }

    /// The inclusive rendered-row range a logical line occupies. One line is
    /// several rows once it wraps, which is the difference between "this line
    /// is on screen" and "this line's first row is on screen".
    pub fn row_span_of_line(&self, line: usize) -> (usize, usize) {
        let first = self.row_of_line(line);
        let last = self
            .wrapped
            .line_to_row
            .get(line + 1)
            .map(|r| r.saturating_sub(1))
            .unwrap_or_else(|| self.visible_row_count().saturating_sub(1));
        (first, last.max(first))
    }

    /// The logical doc line rendered `row` belongs to (clamped to the last
    /// line once `row` runs past the wrapped content).
    fn line_of_row(&self, row: usize) -> usize {
        self.wrapped
            .row_to_line
            .get(row)
            .copied()
            .unwrap_or_else(|| self.content_lines().saturating_sub(1))
    }

    fn max_scroll(&self) -> u16 {
        self.visible_row_count()
            .saturating_sub(self.viewport_h as usize)
            .min(u16::MAX as usize) as u16
    }

    fn clamp_scroll(&mut self) {
        let max = self.max_scroll();
        if self.scroll > max {
            self.scroll = max;
        }
    }

    /// Scroll by `delta` lines. `i32::MIN` jumps home, `i32::MAX` to bottom.
    pub fn scroll_by(&mut self, delta: i32) {
        let max = self.max_scroll();
        self.scroll = match delta {
            i32::MIN => 0,
            i32::MAX => max,
            d => {
                let next = i64::from(self.scroll) + i64::from(d);
                next.clamp(0, i64::from(max)) as u16
            }
        };
        self.keep_cursor_visible();
    }

    /// Drag the cursor along with the viewport so it is never off-screen:
    /// scrolling (wheel, or ctrl+d/u forwarded from the list) used to leave
    /// it behind, so focusing the diff pane showed no cursor at all and the
    /// first `j` jumped somewhere unrelated.
    ///
    /// A live selection is left alone — moving the cursor there would silently
    /// extend the selection.
    fn keep_cursor_visible(&mut self) {
        if self.select_anchor.is_some() {
            return;
        }
        // `scroll`/`viewport_h` are row-space; the cursor is a logical doc
        // line, so clamp in row-space and map the result back to a line.
        let last_row = self.visible_row_count().saturating_sub(1);
        let top = self.scroll as usize;
        let bottom = (top + self.viewport_h.max(1) as usize - 1).min(last_row);
        let cur_row = self.row_of_line(self.cursor_line);
        let clamped_row = cur_row.clamp(top.min(bottom), bottom);
        // Snapping away from the clamp edge keeps the cursor on screen.
        let dir = if clamped_row < cur_row { -1 } else { 1 };
        let clamped = self.snap_off_card(self.line_of_row(clamped_row), dir);
        // A thread card can be taller than the whole pane. When the visible
        // window is *entirely* card lines there is no annotatable line to
        // clamp onto, and `snap_off_card` escapes the viewport — parking the
        // cursor somewhere the user cannot see, in a pane that draws no
        // cursor of its own. Leave it where it is: scrolling through a
        // conversation is reading, not a cursor move. Three-line notes could
        // never reach this, which is why it is new in Phase C.
        // Test the whole row span, not just the first row: a wrapped line
        // starts above the window while most of it is inside, and judging it
        // by its first row alone would strand the cursor on any wrapped diff
        // — notes or no notes.
        let (first_row, last_row_of) = self.row_span_of_line(clamped);
        if last_row_of < top || first_row > bottom {
            return;
        }
        if clamped != self.cursor_line {
            self.cursor_line = clamped;
            self.restyle();
        }
    }

    /// The file line number under the cursor for the header: the new-file
    /// number, or the old one on a deleted line. `None` on a fold or a note
    /// card, which belong to no source line.
    pub fn cursor_file_line(&self) -> Option<u32> {
        let built = self.built.as_ref()?;
        let line = self.doc_to_built(self.cursor_line)?;
        let (old, new) = built.numbers_of_line(line)?;
        new.or(old)
    }

    /// Page relative to the viewport height (`full` = whole page, else half).
    pub fn page(&mut self, down: bool, full: bool) {
        let vh = self.viewport_h.max(1) as i32;
        let amount = if full { vh } else { (vh / 2).max(1) };
        self.scroll_by(if down { amount } else { -amount });
    }

    // ---- direct keys (preview pane focused) ------------------------------

    pub fn on_key(&mut self, ev: crossterm::event::KeyEvent) {
        // The composer owns every keystroke while it is open, so a note can
        // contain any character the keymap would otherwise claim.
        if self.composer.is_some() {
            self.compose_key(ev);
            return;
        }
        let Some(action) = self.keys.action(&ev) else {
            return;
        };
        match action {
            // nvim-style: j/k always move the cursor; the view follows.
            Action::Down | Action::ScrollDown => self.move_cursor(1),
            Action::Up | Action::ScrollUp => self.move_cursor(-1),
            Action::HalfPageDown => self.move_cursor(self.viewport_h.max(2) as i32 / 2),
            Action::HalfPageUp => self.move_cursor(-(self.viewport_h.max(2) as i32) / 2),
            Action::Top | Action::DiffTop => self.move_cursor(i32::MIN),
            Action::Bottom | Action::DiffBottom => self.move_cursor(i32::MAX),
            Action::Select => {
                if matches!(self.state, State::Diff) {
                    self.select_anchor = match self.select_anchor {
                        Some(_) => None,
                        None => {
                            self.flash("visual: j/k extend · a note · esc cancel");
                            Some(self.cursor_line)
                        }
                    };
                    self.restyle();
                }
            }
            Action::Annotate => self.begin_annotate(),
            Action::ToggleThread => self.toggle_thread_at_cursor(),
            Action::NotesView => self.notes_view_request = true,
            // Enter: open the editor, same as Enter over in the list. The
            // list owns that flow (busy lockout, tab-nvim reuse), so just
            // ask it.
            Action::Edit => self.edit_request = true,
            Action::SendNotes => {
                if !self.store.has_unsent() {
                    self.flash(if self.store.is_empty() {
                        "no threads yet — select lines and press a"
                    } else {
                        "nothing unsent — every thread is already with the agent"
                    });
                } else {
                    self.popup_request = Some(PopupReq::PickAgent);
                }
            }
            Action::Quit => {
                // Esc/q first backs out of an active selection.
                if self.select_anchor.is_some() {
                    self.select_anchor = None;
                    self.restyle();
                } else {
                    self.should_quit = true;
                    self.close_view = true;
                }
            }
            _ => {}
        }
    }

    // ---- cursor / selection ----------------------------------------------

    /// Move the cursor line (i32::MIN/MAX = home/end), keep it visible, and
    /// re-apply the selection styling. Only meaningful while selecting (the
    /// cursor is invisible otherwise).
    fn move_cursor(&mut self, delta: i32) {
        let last = self.content_lines().saturating_sub(1);
        let (target, dir) = match delta {
            i32::MIN => (0, 1),
            i32::MAX => (last, -1),
            d => (
                (self.cursor_line as i64 + i64::from(d)).clamp(0, last as i64) as usize,
                if d < 0 { -1 } else { 1 },
            ),
        };
        // Step over a whole card rather than into it.
        self.cursor_line = self.snap_off_card(target, dir);
        // Keep the cursor inside the viewport. `scroll`/`vh` are row-space;
        // convert the cursor's logical line to its row before comparing.
        let vh = self.viewport_h.max(1) as usize;
        let cur_row = self.row_of_line(self.cursor_line);
        if cur_row < self.scroll as usize {
            self.scroll = cur_row as u16;
        } else if cur_row >= self.scroll as usize + vh {
            self.scroll = (cur_row + 1 - vh) as u16;
        }
        self.restyle();
    }

    /// The nearest line that is not part of a note card, searching in `dir`
    /// (-1 back, +1 forward) first and the other way if that runs out.
    ///
    /// Cards are read-only decoration. Letting the cursor land on one means
    /// you can select and annotate your own annotation, which is nonsense —
    /// and the doc↔source mapping has no line to give it either.
    fn snap_off_card(&self, line: usize, dir: i32) -> usize {
        let last = self.content_lines().saturating_sub(1);
        let line = line.min(last);
        if !self.is_card_line(line) {
            return line;
        }
        let forward = (line..=last).find(|i| !self.is_card_line(*i));
        let backward = (0..=line).rev().find(|i| !self.is_card_line(*i));
        let (first, second) = if dir < 0 {
            (backward, forward)
        } else {
            (forward, backward)
        };
        first.or(second).unwrap_or(line)
    }

    /// The selected rendered-line range (anchor..=cursor), or the cursor line.
    pub fn selection(&self) -> (usize, usize) {
        match self.select_anchor {
            Some(a) => (a.min(self.cursor_line), a.max(self.cursor_line)),
            None => (self.cursor_line, self.cursor_line),
        }
    }

    /// Full re-render from the built diff (cards re-injected), then tint.
    /// Use after anything that changes the doc's *content*; plain cursor or
    /// selection moves go through `restyle` alone.
    fn rebuild(&mut self) {
        self.sync_doc();
        self.saved_tint.clear();
        // Cards have just moved (a note was added, edited, deleted, or the
        // pane was resized) and may now sit under the cursor.
        self.cursor_line = self.snap_off_card(self.cursor_line, 1);
        self.restyle();
    }

    /// Tint the cursor line / selection with subtle background colors,
    /// restoring the previously tinted lines first (text colors are never
    /// touched; the tint overrides the red/green line tints while selected,
    /// like an editor would).
    fn restyle(&mut self) {
        // Restore whatever was tinted before.
        for (idx, line) in self.saved_tint.drain(..) {
            if let Some(slot) = self.doc.lines.get_mut(idx) {
                *slot = line;
            }
        }
        if !matches!(self.state, State::Diff) {
            return;
        }
        // The cursor line has to read as "you are here" against the diff's
        // own red/green tints, so it is a real cursorline, not a whisper.
        let cursor_bg = if !self.cfg.theme.is_light() {
            Color::Rgb(0x39, 0x3b, 0x4f)
        } else {
            Color::Rgb(0xdf, 0xe4, 0xee)
        };
        let select_bg = if !self.cfg.theme.is_light() {
            Color::Rgb(0x45, 0x47, 0x5a)
        } else {
            Color::Rgb(0xd8, 0xdd, 0xe6)
        };
        let last = self.doc.lines.len().saturating_sub(1);
        self.cursor_line = self.cursor_line.min(last);
        let tint = |idx: usize,
                    bg: Color,
                    saved: &mut Vec<(usize, ratatui::text::Line<'static>)>,
                    lines: &mut Vec<ratatui::text::Line<'static>>| {
            if let Some(line) = lines.get_mut(idx) {
                saved.push((idx, line.clone()));
                line.style = line.style.bg(bg);
                for span in &mut line.spans {
                    span.style = span.style.bg(bg);
                }
            }
        };
        let mut saved = std::mem::take(&mut self.saved_tint);
        match self.select_anchor {
            Some(_) => {
                let (a, b) = self.selection();
                // Cards inside the range stay untinted — they are not part of
                // the selection and contribute nothing to the note. Collected
                // first so the doc can be borrowed mutably below.
                let rows: Vec<usize> = (a..=b).filter(|i| !self.is_card_line(*i)).collect();
                for idx in rows {
                    tint(idx, select_bg, &mut saved, &mut self.doc.lines);
                }
            }
            None => tint(self.cursor_line, cursor_bg, &mut saved, &mut self.doc.lines),
        }
        self.saved_tint = saved;
        // `doc` just changed (tint applied/moved) — re-wrap so the rendered
        // text (and the row<->line maps scroll/click math relies on) stays
        // in lockstep. `restyle` is the one place every doc mutation ends up
        // going through, content rebuilds included.
        self.sync_wrapped();
    }

    // ---- notes ------------------------------------------------------------

    /// `a`: capture the selection as a pending note and ask the run loop to
    /// open the annotate popup. With no active selection, the note covers the
    /// whole file.
    fn begin_annotate(&mut self) {
        let Some(req) = self.current.clone() else {
            return;
        };
        if req.commit.is_some() || req.scope != Scope::Worktree {
            self.flash("notes work on staged/unstaged changes only");
            return;
        }

        let Some(built) = &self.built else {
            return;
        };
        let (a, b) = self.selection();
        let mut numbers = Vec::new();
        let mut snippet = String::new();
        for line in a..=b {
            let Some(bl) = self.doc_to_built(line) else {
                continue;
            };
            // NEW-side numbers only. `new.or(old)` put an OLD-side number
            // into a field every consumer treats as NEW-side, which was a
            // transient annoyance while notes lived in memory and becomes a
            // permanently wrong anchor once they are persisted. A selection
            // of nothing but deletions therefore has no new-side range, and
            // is rejected below rather than silently mis-anchored.
            if let Some((_, Some(new))) = built.numbers_of_line(bl) {
                numbers.push(new);
            }
            if let Some(text) = built.marker_text_of_line(bl)
                && !text.is_empty()
                && snippet.lines().count() < 40
            {
                snippet.push_str(&text);
                snippet.push('\n');
            }
        }
        // A range that mapped to no source line at all (only cards, or only
        // folds) would silently become a whole-file note — refuse instead.
        let (start, end) = match (numbers.iter().min(), numbers.iter().max()) {
            (Some(&s), Some(&e)) if s > 0 => (s, e),
            _ => {
                self.flash("select added or context lines to annotate");
                return;
            }
        };
        self.pending = Some(Pending {
            anchor: self.anchor_at(req.file, start, end, snippet, req.cached),
            target: Target::New,
        });
        self.composer = Some(Composer::new(String::new(), None));
        self.rebuild();
        self.scroll_to_composer();
    }

    /// Build an anchor, stamping it with the blob and HEAD it was written
    /// against so `thread::reanchor` can follow the code when the agent
    /// rewrites it.
    fn anchor_at(
        &self,
        file: PathBuf,
        start: u32,
        end: u32,
        snippet: String,
        cached: bool,
    ) -> Anchor {
        let blob = self.repo.hash_object(&file);
        let head_sha = self.repo.head_sha();
        Anchor {
            file,
            start,
            end,
            cached,
            snippet,
            blob,
            head_sha,
        }
    }

    /// Hand a key to the open composer and act on what it asks for.
    fn compose_key(&mut self, ev: crossterm::event::KeyEvent) {
        let width = self.composer_width();
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        match composer.key(ev, width) {
            Outcome::Ignored => {}
            Outcome::Edited => {
                self.rebuild();
                self.scroll_to_composer();
            }
            Outcome::Save => self.commit_composer(),
            Outcome::Cancel => self.cancel_composer(),
        }
    }

    /// Text width inside the composer box (the card's own inner width).
    fn composer_width(&self) -> usize {
        card::card_text_width(self.viewport_w.max(MIN_CARD_WIDTH) as usize)
    }

    /// Enter in the composer: save a new note or the edit of an existing one.
    /// An empty note is a cancel — there is nothing to send an agent.
    fn commit_composer(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let Some(text) = composer.finish() else {
            // An empty note is a cancel — there is nothing to send an agent.
            self.pending = None;
            self.select_anchor = None;
            self.rebuild();
            return;
        };
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.select_anchor = None;
        match pending.target {
            Target::New => {
                let id = self.store.alloc_id();
                self.store
                    .threads
                    .push(Thread::new(id, pending.anchor, text));
            }
            Target::Edit { id, turn } => {
                let Some(thread) = self.store.get_mut(id) else {
                    return;
                };
                match thread.turns.get_mut(turn) {
                    // A turn that was delivered while the composer was open
                    // is history; appending is the only honest outcome.
                    Some(t) if t.sent => thread.push(Author::Human, text),
                    Some(t) => {
                        t.text = text;
                        t.at = crate::thread::now_epoch();
                    }
                    None => thread.push(Author::Human, text),
                }
            }
            Target::Reply(id) => {
                let Some(thread) = self.store.get_mut(id) else {
                    return;
                };
                thread.push(Author::Human, text);
            }
        }
        self.threads_changed();
    }

    fn cancel_composer(&mut self) {
        self.composer = None;
        self.pending = None;
        self.select_anchor = None;
        self.rebuild();
    }

    /// Open the composer on an existing note (asked for by the notes view).
    /// Returns false when the note isn't in the shown file, so the caller can
    /// fall back rather than silently doing nothing.
    /// Reopens the last *unsent* Human turn. Once a turn has been delivered
    /// it is part of the conversation's history, so editing becomes a reply
    /// instead of a rewrite.
    pub fn begin_edit_note(&mut self, id: u64) -> bool {
        let Some(thread) = self.store.get(id) else {
            return false;
        };
        let anchor = thread.anchor.clone();
        let (seed, target) = match thread.editable_turn() {
            Some(turn) => (thread.turns[turn].text.clone(), Target::Edit { id, turn }),
            None => (String::new(), Target::Reply(id)),
        };
        self.select_anchor = None;
        self.composer = Some(Composer::new(seed, Some(id)));
        self.pending = Some(Pending { anchor, target });
        self.rebuild();
        self.scroll_to_composer();
        true
    }

    /// Open the composer to add a reply turn to an existing conversation.
    pub fn begin_reply(&mut self, id: u64) -> bool {
        let Some(thread) = self.store.get(id) else {
            return false;
        };
        let anchor = thread.anchor.clone();
        self.select_anchor = None;
        self.composer = Some(Composer::new(String::new(), Some(id)));
        self.pending = Some(Pending {
            anchor,
            target: Target::Reply(id),
        });
        self.rebuild();
        self.scroll_to_composer();
        true
    }

    /// Open the composer for a whole-file note (asked for by the file list).
    pub fn begin_file_note(&mut self) {
        let Some(req) = self.current.clone() else {
            return;
        };
        self.select_anchor = None;
        self.pending = Some(Pending {
            anchor: self.anchor_at(req.file, 0, 0, String::new(), req.cached),
            target: Target::New,
        });
        self.composer = Some(Composer::new(String::new(), None));
        self.rebuild();
        self.scroll_to_composer();
    }

    /// The Show that puts a note's own file on screen, so the composer can
    /// open on it without the list having to say which file that is.
    pub fn show_for_note(&self, id: u64) -> Option<crate::ipc::ToPreview> {
        let thread = self.store.get(id)?;
        if self.current.as_ref().map(|r| &r.file) == Some(&thread.anchor.file) {
            return None; // already showing it
        }
        Some(crate::ipc::ToPreview::Show {
            file: thread.anchor.file.clone(),
            orig_path: None,
            scope: Scope::Worktree,
            cached: thread.anchor.cached,
            kind: crate::git::ChangeKind::Modified,
            commit: None,
        })
    }

    /// Scroll so the whole composer box is on screen, preferring to keep its
    /// top visible when it is taller than the viewport.
    fn scroll_to_composer(&mut self) {
        let Some((start, len)) = self.composer_span else {
            return;
        };
        let vh = self.viewport_h.max(1) as usize;
        let last_line = (start + len)
            .saturating_sub(1)
            .min(self.content_lines().saturating_sub(1));
        let start_row = self.row_of_line(start);
        let end_row = self.row_of_line(last_line) + 1; // one past the box's last row
        if end_row > self.scroll as usize + vh {
            self.scroll = (end_row - vh) as u16;
        }
        if (self.scroll as usize) > start_row {
            self.scroll = start_row as u16;
        }
        self.clamp_scroll();
    }

    /// The annotate popup returned text: open a thread at the pending anchor.
    pub fn finish_annotate(&mut self, text: String) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let id = self.store.alloc_id();
        self.store
            .threads
            .push(Thread::new(id, pending.anchor, text));
        self.select_anchor = None;
        self.threads_changed();
    }

    /// Mark every thread that just had turns delivered. Replaces the old
    /// clear-on-send: the conversation survives so a reply can land in it.
    /// Re-render and re-broadcast without changing any thread. Used when
    /// something outside the store changes how threads should read — queuing
    /// a send, for instance.
    pub fn redraw_threads(&mut self) {
        self.threads_changed();
    }

    /// Exactly the turns that were asked about are marked sent — not every
    /// Human turn in the thread. A request can sit in the worker's queue
    /// behind a multi-minute one, and the thread may gain a turn in the
    /// meantime; marking that one sent would retire text nobody asked about.
    ///
    /// `mark_sent` is not used for the same reason: it marks all of them.
    pub fn mark_turns_sent(&mut self, id: u64, turns: &[usize], agent: &crate::thread::AgentRef) {
        let Some(t) = self.store.get_mut(id) else {
            return;
        };
        for &i in turns {
            if let Some(turn) = t.turns.get_mut(i) {
                turn.sent = true;
            }
        }
        t.agent = Some(agent.clone());
        // An answered thread that has since been asked a follow-up must not
        // be dragged back to `Sent` by a late delivery for the earlier turn.
        if t.state != crate::thread::ThreadState::Answered {
            t.state = crate::thread::ThreadState::Sent;
        }
        t.updated_at = crate::thread::now_epoch();
        self.threads_changed();
    }

    /// Delivery or capture failed. Records it *in the thread* — a flash ages
    /// out in three seconds and leaves no way to tell a stranded thread from
    /// one the agent is still working on.
    ///
    /// `still_pending` means the question provably never reached the agent,
    /// so exactly the turns this attempt claimed are handed back — and only
    /// those. Clearing every Human turn would also unmark ones delivered by
    /// an *earlier*, successful send and silently re-ask them.
    ///
    /// The undo is needed because `Delivered` is optimistic: it fires before
    /// herdr has accepted the prompt, so a refusal can arrive after the turns
    /// were already marked sent.
    ///
    /// Otherwise the question may be in front of the agent already, so the
    /// thread goes to `Failed` — still resendable, because that is the user's
    /// call, but no longer indistinguishable from one being worked on.
    pub fn mark_thread_failed(&mut self, id: u64, turns: &[usize], err: &str, still_pending: bool) {
        let Some(t) = self.store.get_mut(id) else {
            return;
        };
        t.push(crate::thread::Author::System, err.to_string());
        if still_pending {
            for &i in turns {
                if let Some(turn) = t.turns.get_mut(i) {
                    turn.sent = false;
                }
            }
            if t.has_unsent() {
                t.state = crate::thread::ThreadState::Draft;
            }
        } else {
            t.state = crate::thread::ThreadState::Failed;
        }
        self.threads_changed();
    }

    /// Append an agent's reply to the thread that asked for it.
    pub fn append_reply(&mut self, id: u64, text: String) {
        if let Some(thread) = self.store.get_mut(id) {
            thread.push(Author::Agent, text);
            self.threads_changed();
        }
    }

    /// Add a follow-up question to an existing thread. `push` walks a
    /// delivered thread back to `Draft`, so it becomes pending again.
    pub fn append_human_turn(&mut self, id: u64, text: String) {
        if let Some(thread) = self.store.get_mut(id) {
            thread.push(Author::Human, text);
            self.threads_changed();
        }
    }

    pub fn delete_note(&mut self, id: u64) {
        if self.store.remove(id) {
            self.threads_changed();
        }
    }

    /// The notes view hovered note `idx`: scroll its card into view when its
    /// file is already shown, else remember it until that diff arrives.
    pub fn focus_note(&mut self, id: u64) {
        let thread = self.store.get(id);
        let same_file = match (thread, &self.current) {
            (Some(t), Some(req)) => t.anchor.file == req.file,
            _ => false,
        };
        if same_file && matches!(self.state, State::Diff) {
            self.scroll_to_note(id);
        } else {
            self.pending_focus = Some(id);
        }
    }

    fn scroll_to_note(&mut self, id: u64) {
        let Some((_, start, len)) = self.card_starts.iter().find(|(n, _, _)| *n == id) else {
            return; // not in the shown file, or its card is the open composer
        };
        let (start, len) = (*start, *len);
        let vh = self.viewport_h.max(1) as usize;
        // Two-sided, biased to the bottom: the newest turn is what you came
        // to read, so when a conversation is taller than the pane the *end*
        // of the card must be on screen, not three lines of its header.
        let start_row = self.row_of_line(start).saturating_sub(3);
        let last_line = (start + len)
            .saturating_sub(1)
            .min(self.content_lines().saturating_sub(1));
        let end_row = self.row_of_line(last_line) + 1;
        self.scroll = if end_row > start_row + vh {
            end_row.saturating_sub(vh) as u16
        } else {
            start_row as u16
        };
        self.clamp_scroll();
        self.keep_cursor_visible();
    }

    /// The thread whose card contains `line`, if any: the nearest card
    /// starting at or above `line` whose recorded height still covers it.
    fn thread_at_line(&self, line: usize) -> Option<u64> {
        let (id, start, len) = self
            .card_starts
            .iter()
            .filter(|(_, start, _)| *start <= line)
            .max_by_key(|(_, start, _)| *start)?;
        (line < start + len).then_some(*id)
    }

    /// `z`: collapse a long conversation to its newest turn, or expand it
    /// again. The cursor can never rest *on* a card, so this falls back to the
    /// nearest card below and then to the nearest above. The upward fallback
    /// is not a nicety: a whole-file note — and every thread whose anchor was
    /// lost when the agent rewrote the file — splices at doc line 0, with the
    /// cursor parked permanently below it.
    fn toggle_thread_at_cursor(&mut self) {
        let id = self
            .thread_at_line(self.cursor_line)
            .or_else(|| self.nearest_card_below(self.cursor_line))
            .or_else(|| self.nearest_card_above(self.cursor_line));
        let Some(id) = id else {
            self.flash("no thread here");
            return;
        };
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        self.rebuild();
    }

    /// The first thread card starting at or after `line`.
    fn nearest_card_below(&self, line: usize) -> Option<u64> {
        self.card_starts
            .iter()
            .filter(|(_, start, _)| *start >= line)
            .min_by_key(|(_, start, _)| *start)
            .map(|(id, _, _)| *id)
    }

    /// The last thread card starting before `line`.
    fn nearest_card_above(&self, line: usize) -> Option<u64> {
        self.card_starts
            .iter()
            .filter(|(_, start, _)| *start < line)
            .max_by_key(|(_, start, _)| *start)
            .map(|(id, _, _)| *id)
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), std::time::Instant::now()));
    }

    /// The flash message if still fresh (3 s TTL).
    pub fn active_flash(&self) -> Option<&str> {
        match &self.flash {
            Some((msg, at)) if at.elapsed() < std::time::Duration::from_secs(3) => {
                Some(msg.as_str())
            }
            _ => None,
        }
    }
}
