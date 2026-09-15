//! Preview-pane behaviour + render tests. `PreviewApp` owns no terminal or
//! sockets, so we can drive it directly and render into a `TestBackend`.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

use herdr_gitview::config::Config;
use herdr_gitview::git::{ChangeKind, Repo, Scope};
use herdr_gitview::keymap::Keymap;
use herdr_gitview::preview::app::State;
use herdr_gitview::preview::highlight::Highlighter;
use herdr_gitview::preview::{PreviewApp, ShowReq, render, ui};

const W: u16 = 60;
const H: u16 = 12;

fn app() -> PreviewApp {
    let cfg = Config::default();
    let keys = Keymap::build(&HashMap::new()).unwrap();
    let repo = Repo {
        root: PathBuf::from("."),
    };
    PreviewApp::new(cfg, repo, keys, store_path())
}

/// A throwaway store path per test process, so persistence writes land
/// somewhere harmless instead of the user's real plugin state dir.
fn store_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "gitview-test-{}-{:?}.json",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn req(file: &str) -> ShowReq {
    ShowReq {
        file: PathBuf::from(file),
        orig_path: None,
        scope: Scope::Worktree,
        cached: false,
        kind: ChangeKind::Modified,
        commit: None,
    }
}

/// A built diff doc with `n` inserted lines (plain-text path → no syntax
/// highlighting cost, even at 20k lines).
fn fake_diff(n: usize) -> render::DiffDoc {
    let hl = Highlighter::new(herdr_gitview::config::Theme::Dark);
    let new: String = (0..n).map(|i| format!("line {i}\n")).collect();
    render::build(
        &PathBuf::from("f.txt"),
        "",
        &new,
        &hl,
        herdr_gitview::config::Theme::Dark,
        3,
        4,
    )
}

/// A built doc for identical content (the "no changes" case).
fn empty_diff() -> render::DiffDoc {
    let hl = Highlighter::new(herdr_gitview::config::Theme::Dark);
    render::build(
        &PathBuf::from("f.txt"),
        "same\n",
        "same\n",
        &hl,
        herdr_gitview::config::Theme::Dark,
        3,
        4,
    )
}

fn draw(app: &mut PreviewApp) -> Buffer {
    draw_at(app, W, H)
}

/// Render at an explicit size — card titles have to survive panes narrower
/// than the 60 columns the rest of these tests use.
fn draw_at(app: &mut PreviewApp, w: u16, h: u16) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn row_at(buf: &Buffer, y: u16, w: u16) -> String {
    (0..w).map(|x| buf[(x, y)].symbol()).collect()
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..W).map(|x| buf[(x, y)].symbol()).collect()
}

#[test]
fn splash_before_any_diff() {
    let mut a = app();
    let buf = draw(&mut a);
    let body: String = (1..H - 1).map(|y| row(&buf, y)).collect();
    assert!(body.contains("waiting for file list"), "body: {body:?}");
}

#[test]
fn shows_diff_and_header_path() {
    let mut a = app();
    let r = req("src/main.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(5)));

    let buf = draw(&mut a);
    let header = row(&buf, 0);
    assert!(header.contains("src/main.rs"), "header: {header:?}");
    assert!(header.contains("uncommitted"), "header: {header:?}");

    let body: String = (1..H - 1).map(|y| row(&buf, y)).collect();
    assert!(body.contains("line 0"), "body: {body:?}");
}

#[test]
fn stale_diff_result_is_dropped() {
    let mut a = app();
    let first = req("a.rs");
    a.begin_show(first.clone());
    // Selection moves on before the first diff returns.
    let second = req("b.rs");
    a.begin_show(second.clone());
    // Late result for the old request must be ignored.
    a.apply_diff(&first, Ok(fake_diff(3)));
    assert!(matches!(a.state, State::Splash(_) | State::Empty));

    // The current request's result is accepted.
    a.apply_diff(&second, Ok(fake_diff(3)));
    assert!(matches!(a.state, State::Diff));
}

#[test]
fn empty_diff_shows_no_changes() {
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(empty_diff()));
    assert!(matches!(a.state, State::Empty));

    let buf = draw(&mut a);
    let body: String = (1..H - 1).map(|y| row(&buf, y)).collect();
    assert!(body.contains("no changes"), "body: {body:?}");
}

#[test]
fn diff_error_shows_message() {
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Err("fatal: bad revision".to_string()));

    let buf = draw(&mut a);
    let body: String = (0..H).map(|y| row(&buf, y)).collect();
    assert!(body.contains("bad revision"), "body: {body:?}");
}

#[test]
fn scroll_is_clamped_and_bottom_jumps() {
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(30)));
    // Render once so the app learns its viewport height.
    draw(&mut a);

    a.scroll_by(-5); // can't go above the top
    assert_eq!(a.scroll, 0);

    a.scroll_by(i32::MAX); // jump to bottom
    let bottom = a.scroll;
    assert!(bottom > 0, "expected a scrollable diff");

    a.scroll_by(1000); // clamped at bottom
    assert_eq!(a.scroll, bottom);

    a.scroll_by(i32::MIN); // home
    assert_eq!(a.scroll, 0);
}

#[test]
fn truncation_notice_when_capped() {
    let mut a = app();
    let r = req("big.rs");
    a.begin_show(r.clone());
    // 20_001 content lines → over the 20k cap.
    a.apply_diff(&r, Ok(fake_diff(20_001)));
    a.scroll_by(i32::MAX);

    let buf = draw(&mut a);
    let body: String = (0..H).map(|y| row(&buf, y)).collect();
    assert!(body.contains("diff truncated"), "body: {body:?}");
}

// ---- "where am I": the cursor line ----------------------------------------

/// The background color of the first cell of body row `y`.
fn bg_at(buf: &Buffer, y: u16) -> ratatui::style::Color {
    buf[(0, y)]
        .style()
        .bg
        .unwrap_or(ratatui::style::Color::Reset)
}

/// The body row the cursor is tinted on, found by the odd-one-out background.
fn cursor_row(buf: &Buffer) -> Option<u16> {
    let rows: Vec<(u16, ratatui::style::Color)> = (1..H - 1).map(|y| (y, bg_at(buf, y))).collect();
    // The cursor tint is whichever background appears exactly once.
    rows.iter()
        .find(|(_, c)| rows.iter().filter(|(_, o)| o == c).count() == 1)
        .map(|(y, _)| *y)
}

#[test]
fn the_cursor_line_is_visibly_tinted() {
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(30)));
    let buf = draw(&mut a);

    assert_eq!(cursor_row(&buf), Some(1), "cursor starts on the first line");
    // and it is a real tint, not the default background
    assert_ne!(bg_at(&buf, 1), ratatui::style::Color::Reset);
    assert_ne!(bg_at(&buf, 1), bg_at(&buf, 2), "cursor row must stand out");
}

#[test]
fn the_cursor_stays_on_screen_when_the_diff_is_scrolled() {
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(60)));
    draw(&mut a); // learn the viewport height

    // Scrolling from the list pane (ctrl+d / wheel) used to leave the cursor
    // behind, so the diff pane showed no cursor at all.
    a.scroll_by(20);
    assert!(
        a.cursor_line >= a.scroll as usize,
        "cursor above the viewport"
    );
    assert!(
        a.cursor_line < a.scroll as usize + a.viewport_h as usize,
        "cursor below the viewport"
    );
    let buf = draw(&mut a);
    assert!(
        cursor_row(&buf).is_some(),
        "no cursor visible after scrolling"
    );

    // ...including a jump to the very bottom, and back to the top.
    a.scroll_by(i32::MAX);
    assert!(a.cursor_line >= a.scroll as usize);
    assert!(draw(&mut a).area.height > 0 && cursor_row(&draw(&mut a)).is_some());

    a.scroll_by(i32::MIN);
    assert!(a.cursor_line < a.viewport_h as usize);
    assert!(cursor_row(&draw(&mut a)).is_some());
}

#[test]
fn scrolling_does_not_drag_the_cursor_through_a_live_selection() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(60)));
    draw(&mut a);

    a.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)); // start selecting
    let anchored = a.cursor_line;
    a.scroll_by(30);
    assert_eq!(
        a.cursor_line, anchored,
        "scrolling must not silently extend the selection"
    );
}

#[test]
fn the_header_reports_the_cursor_line_not_the_scroll_offset() {
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(40)));
    draw(&mut a);

    let header = row(&draw(&mut a), 0);
    assert!(header.contains("ln 1"), "header: {header:?}");

    // Move the cursor down a few lines; the header follows it.
    for _ in 0..5 {
        a.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('j'),
            crossterm::event::KeyModifiers::NONE,
        ));
    }
    let header = row(&draw(&mut a), 0);
    assert!(header.contains("ln 6"), "header: {header:?}");
    assert_eq!(a.cursor_file_line(), Some(6));
}

// ---- note cards in the diff ------------------------------------------------

fn note(id: u64, start: u32, end: u32, text: &str) -> herdr_gitview::thread::Thread {
    herdr_gitview::thread::Thread::new(
        id,
        herdr_gitview::thread::Anchor {
            file: PathBuf::from("f.txt"),
            start,
            end,
            cached: false,
            snippet: String::new(),
            blob: None,
            head_sha: None,
        },
        text.to_string(),
    )
}

/// A thread that has been sent and answered, so it renders as a conversation.
fn conversation(id: u64, line: u32, question: &str, answer: &str) -> herdr_gitview::thread::Thread {
    let mut t = note(id, line, line, question);
    t.mark_sent(herdr_gitview::thread::AgentRef {
        pane: "w1:p1".into(),
        agent: "claude".into(),
        session: None,
        session_kind: None,
    });
    t.push(herdr_gitview::thread::Author::Agent, answer.to_string());
    t
}

/// A diff with `n` inserted lines plus the given notes, rendered once.
fn app_with_notes(notes: Vec<herdr_gitview::thread::Thread>) -> PreviewApp {
    let mut a = app();
    let r = req("f.txt");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(10)));
    a.store.threads = notes;
    a.apply_diff(&r, Ok(fake_diff(10))); // re-sync the doc with the notes
    draw(&mut a);
    a
}

#[test]
fn a_note_renders_as_a_boxed_card_under_its_line() {
    let mut a = app_with_notes(vec![note(1, 3, 3, "use a constant")]);
    let buf = draw(&mut a);
    let body: Vec<String> = (1..H - 1).map(|y| row(&buf, y)).collect();

    let top = body
        .iter()
        .position(|l| l.contains('╭'))
        .expect("no card top");
    assert!(
        body[top].contains("note · line 3"),
        "title: {:?}",
        body[top]
    );
    assert!(
        body[top + 1].contains("use a constant"),
        "body: {:?}",
        body[top + 1]
    );
    assert!(
        body[top + 2].contains('╰'),
        "no card bottom: {:?}",
        body[top + 2]
    );
    // The card sits *under* the line it comments on.
    assert!(
        body[top - 1].contains("line 2"),
        "anchor: {:?}",
        body[top - 1]
    );
}

#[test]
fn a_conversation_renders_each_turn_under_its_speaker() {
    let mut a = app_with_notes(vec![conversation(
        1,
        3,
        "why is this safe?",
        "the caller checks it first.",
    )]);
    let buf = draw(&mut a);
    let body: Vec<String> = (1..H - 1).map(|y| row(&buf, y)).collect();
    let top = body.iter().position(|l| l.contains('╭')).expect("no card");

    // Title carries the turn count and the state badge.
    assert!(body[top].contains("2 turns"), "title: {:?}", body[top]);
    assert!(body[top].contains("replied"), "title: {:?}", body[top]);

    // Both turns are present, each under its own speaker, in order.
    let card = body[top..].join("\n");
    let you = card.find("you").expect("no human role label");
    let q = card.find("why is this safe?").expect("no question");
    let claude = card.find("claude").expect("agent named by its own name");
    let an = card.find("the caller checks it first.").expect("no answer");
    assert!(
        you < q && q < claude && claude < an,
        "out of order:\n{card}"
    );
}

#[test]
fn collapsing_a_thread_leaves_only_its_newest_turn() {
    let mut a = app_with_notes(vec![conversation(
        1,
        3,
        "why is this safe?",
        "the caller checks it first.",
    )]);
    // `z` on the card below the cursor.
    a.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('z'),
        crossterm::event::KeyModifiers::NONE,
    ));
    let buf = draw(&mut a);
    let body: Vec<String> = (1..H - 1).map(|y| row(&buf, y)).collect();
    let card = body.join("\n");

    assert!(
        card.contains("the caller checks it first."),
        "the newest turn stays visible:\n{card}"
    );
    assert!(
        !card.contains("why is this safe?"),
        "the older turn is hidden:\n{card}"
    );
    assert!(
        card.contains("1 earlier turn"),
        "and the card says what it hid:\n{card}"
    );
}

/// Regression: a thread card can be taller than the whole pane. Scrolling
/// into its middle leaves a viewport made *entirely* of card lines, and
/// `keep_cursor_visible` then had nothing to clamp onto — it snapped the
/// cursor clear of the card and parked it off-screen, in a pane that draws no
/// cursor of its own. Three-line notes could never reach this.
#[test]
fn scrolling_inside_a_tall_thread_does_not_fling_the_cursor_off_screen() {
    let mut t = conversation(1, 3, "opening question", "first answer");
    for i in 0..8 {
        t.push(
            herdr_gitview::thread::Author::Human,
            format!("follow up {i}"),
        );
        t.push(herdr_gitview::thread::Author::Agent, format!("answer {i}"));
    }
    let mut a = app_with_notes(vec![t]);
    draw(&mut a);

    let body_h = a.viewport_h as usize;
    let before = a.cursor_line;
    // Scroll so the visible window is entirely inside the card's interior.
    a.scroll_by(5);
    draw(&mut a);

    let top = a.scroll as usize;
    let visible = top..top + body_h;
    assert!(
        a.cursor_line == before || visible.contains(&a.cursor_line),
        "cursor flung to {} from {}, window {:?}",
        a.cursor_line,
        before,
        visible
    );
}

/// The title has to fit a 60-column pane once it carries a turn count, a
/// state badge and possibly "anchor lost". Eliding the tail would drop
/// exactly the part that says the thread no longer points at real code, so
/// the least important badge goes first and the range label shrinks.
#[test]
fn a_narrow_card_drops_the_turn_count_before_the_anchor_warning() {
    // `end` is past the end of the diff, so the anchor cannot be found.
    let mut t = conversation(1, 3, "why is this safe?", "the caller checks.");
    t.anchor.start = 12;
    t.anchor.end = 99;
    let mut a = app_with_notes(vec![t]);
    let buf = draw(&mut a);
    let body: Vec<String> = (1..H - 1).map(|y| row(&buf, y)).collect();
    let title = body
        .iter()
        .find(|l| l.contains('╭'))
        .expect("no card")
        .clone();

    assert!(
        title.contains("anchor lost"),
        "the anchor warning must survive a narrow pane: {title:?}"
    );
    assert!(
        title.contains("replied"),
        "so must the state badge: {title:?}"
    );
    assert!(
        !title.contains("2 turns"),
        "the turn count is what gives way: {title:?}"
    );
}

/// Regression: two threads on one anchor are spliced back-to-back, so their
/// doc lines form a single unbroken run. Deriving a card's height by walking
/// that run merged them, and focusing the first scrolled to the end of the
/// second. Two whole-file notes, or any two anchor-lost threads, both land on
/// anchor 0 — so this is reachable without ever putting two notes on one line.
#[test]
fn focusing_one_of_two_threads_on_the_same_line_shows_that_thread() {
    let short = note(1, 3, 3, "FIRSTCARD");
    let mut long = conversation(2, 3, "why?", "because.");
    for i in 0..6 {
        long.push(herdr_gitview::thread::Author::Human, format!("more {i}"));
        long.push(herdr_gitview::thread::Author::Agent, format!("reply {i}"));
    }
    let mut a = app_with_notes(vec![short, long]);
    draw(&mut a);

    a.focus_note(1);
    let buf = draw(&mut a);
    let screen: String = (1..H - 1).map(|y| row(&buf, y)).collect();
    assert!(
        screen.contains("FIRSTCARD"),
        "focusing thread 1 scrolled somewhere else entirely:\n{screen}"
    );
}

/// Regression: a whole-file note's card is spliced at doc line 0, so the
/// cursor is always *below* it and a downward-only search could never find
/// it. Same for every thread whose anchor was lost — which is the normal
/// outcome of the agent actually applying the fix.
#[test]
fn z_folds_a_whole_file_thread_even_though_its_card_is_above_the_cursor() {
    let mut t = conversation(1, 0, "look at the whole file", "had a look.");
    t.anchor.start = 0;
    t.anchor.end = 0; // whole-file: card splices at doc line 0
    let mut a = app_with_notes(vec![t]);
    draw(&mut a);
    assert!(a.cursor_line > 0, "cursor sits below the card");

    a.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('z'),
        crossterm::event::KeyModifiers::NONE,
    ));
    let buf = draw(&mut a);
    let screen: String = (1..H - 1).map(|y| row(&buf, y)).collect();
    assert!(
        screen.contains("1 earlier turn"),
        "z did not fold the card above the cursor:\n{screen}"
    );
}

/// Regression: the title dropped badges by testing only the label's minimum
/// width, so it kept discarding them after everything already fitted. The
/// state badge went first, leaving a card that no longer said whether the
/// agent had replied.
#[test]
fn a_title_keeps_every_badge_that_still_fits() {
    // 56 columns: dropping the turn count alone makes the whole title fit,
    // but the label is still shorter than the MIN_LABEL floor — so a loop
    // that only consults that floor carries on and discards "replied" too.
    const NARROW: u16 = 56;
    let mut t = conversation(1, 5, "why?", "because.");
    t.anchor.start = 5;
    t.anchor.end = 99; // past the diff: adds the "anchor lost" badge
    let mut a = app_with_notes(vec![t]);
    let buf = draw_at(&mut a, NARROW, H);
    let title = (1..H - 1)
        .map(|y| row_at(&buf, y, NARROW))
        .find(|l| l.contains('╭'))
        .expect("no card");
    assert!(
        title.contains("anchor lost"),
        "the anchor warning must survive: {title:?}"
    );
    assert!(
        title.contains("replied"),
        "the state badge still fits here and must be kept: {title:?}"
    );
}

/// Regression: the tall-card guard judged a line by its *first* rendered row.
/// A wrapped line starts above the window while most of it is on screen, so
/// the guard bailed and left the cursor stranded — on any wrapped diff, with
/// no notes involved at all.
#[test]
fn scrolling_a_wrapped_diff_with_no_notes_still_drags_the_cursor_along() {
    let mut a = app();
    let r = req("x.rs");
    a.begin_show(r.clone());
    // Lines far wider than the pane, so each wraps to several rows.
    let hl = Highlighter::new(herdr_gitview::config::Theme::Dark);
    let new: String = (0..40)
        .map(|i| format!("line {i} {}\n", "x".repeat(200)))
        .collect();
    a.apply_diff(
        &r,
        Ok(render::build(
            &PathBuf::from("x.rs"),
            "",
            &new,
            &hl,
            herdr_gitview::config::Theme::Dark,
            3,
            4,
        )),
    );
    draw(&mut a);

    a.scroll_by(6);
    draw(&mut a);
    let top = a.scroll as usize;
    let bottom = top + a.viewport_h as usize;
    let (first, last) = a.row_span_of_line(a.cursor_line);
    assert!(
        last >= top && first < bottom,
        "cursor line {} occupies rows {}..={}, window {}..{}",
        a.cursor_line,
        first,
        last,
        top,
        bottom
    );
}

#[test]
fn a_multi_line_note_keeps_its_lines_inside_one_card() {
    let mut a = app_with_notes(vec![note(1, 2, 2, "first thought\nsecond thought")]);
    let buf = draw(&mut a);
    let body: Vec<String> = (1..H - 1).map(|y| row(&buf, y)).collect();

    let top = body.iter().position(|l| l.contains('╭')).expect("no card");
    assert!(body[top + 1].contains("first thought"));
    assert!(body[top + 2].contains("second thought"));
    assert!(body[top + 3].contains('╰'), "one box around both lines");
    // ...and exactly one box.
    assert_eq!(body.iter().filter(|l| l.contains('╭')).count(), 1);
}

#[test]
fn an_empty_note_still_draws_a_box() {
    let mut a = app_with_notes(vec![note(1, 2, 2, "")]);
    let buf = draw(&mut a);
    let body: Vec<String> = (1..H - 1).map(|y| row(&buf, y)).collect();
    let top = body.iter().position(|l| l.contains('╭')).expect("no card");
    assert!(body[top + 1].contains('│'));
    assert!(body[top + 2].contains('╰'));
}

#[test]
fn the_commented_line_gets_an_accented_gutter() {
    let mut a = app_with_notes(vec![note(1, 4, 4, "look here")]);
    let buf = draw(&mut a);
    // Find the row for new-line 4 and check its line-number cell is accented.
    let y = (1..H - 1)
        .find(|y| row(&buf, *y).contains("line 3")) // "line 3" is new-file line 4
        .expect("anchor row not visible");
    // Compare against the palette constant, not a literal hue: the accent is
    // deliberately RGB so a stylised terminal theme cannot reassign it.
    let accent = Some(herdr_gitview::palette::ACCENT);
    let accented = (0..8).any(|x| buf[(x, y)].style().fg == accent);
    assert!(accented, "gutter of the commented line is not accented");
    // An uncommented row is not accented.
    let other = (1..H - 1)
        .find(|y| row(&buf, *y).contains("line 6"))
        .expect("other row");
    assert!(!(0..8).any(|x| buf[(x, other)].style().fg == accent));
}

#[test]
fn multi_line_cards_do_not_break_the_line_number_mapping() {
    // Every card line must be excluded from the doc→source mapping, or the
    // header and note anchors drift once a card is more than one line tall.
    let mut a = app_with_notes(vec![
        note(1, 2, 2, "one\ntwo\nthree"),
        note(2, 5, 5, "another\nmulti-line note"),
    ]);
    draw(&mut a);

    // Walk the whole doc: every line is either a card line (no source line)
    // or maps to a source line, and the source lines only ever increase.
    let mut last = 0;
    for i in 0..a.doc.lines.len() {
        a.cursor_line = i;
        if let Some(no) = a.cursor_file_line() {
            assert!(no >= last, "line numbers went backwards at doc line {i}");
            last = no;
        }
    }
    assert_eq!(last, 10, "should reach the last line of the file");
}

#[test]
fn cards_are_re_boxed_when_the_pane_width_changes() {
    let mut a = app_with_notes(vec![note(1, 2, 2, "a note that is long enough to wrap")]);
    let wide = draw(&mut a);
    let wide_top = (1..H - 1)
        .map(|y| row(&wide, y))
        .find(|l| l.contains('╭'))
        .expect("no card");

    // Redraw into a narrower terminal.
    let mut term = Terminal::new(TestBackend::new(34, H)).unwrap();
    term.draw(|f| ui::render(f, &mut a)).unwrap();
    let narrow = term.backend().buffer().clone();
    let narrow_top = (1..H - 1)
        .map(|y| (0..34).map(|x| narrow[(x, y)].symbol()).collect::<String>())
        .find(|l| l.contains('╭'))
        .expect("no card when narrow");

    assert!(narrow_top.trim_end().width() < wide_top.trim_end().width());
    assert!(
        narrow_top.contains('╮'),
        "the box must still close: {narrow_top:?}"
    );
}

// ---- cards are decoration, not content -------------------------------------

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

fn press(a: &mut PreviewApp, c: char) {
    a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
}

/// Is the cursor sitting on a line that belongs to a note card?
fn on_card(a: &PreviewApp) -> bool {
    let line = &a.doc.lines[a.cursor_line];
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    text.contains('╭') || text.contains('│') || text.contains('╰')
}

#[test]
fn the_cursor_steps_over_a_card_instead_of_into_it() {
    let mut a = app_with_notes(vec![note(1, 3, 3, "a\nmulti\nline\nnote")]);
    // Walk the whole document downwards, then back up.
    for _ in 0..a.doc.lines.len() + 2 {
        press(&mut a, 'j');
        assert!(
            !on_card(&a),
            "cursor landed inside a card at {}",
            a.cursor_line
        );
    }
    for _ in 0..a.doc.lines.len() + 2 {
        press(&mut a, 'k');
        assert!(
            !on_card(&a),
            "cursor landed inside a card at {}",
            a.cursor_line
        );
    }
}

#[test]
fn home_and_end_skip_cards_at_the_edges() {
    // A whole-file note puts a card at the very top of the document.
    let mut a = app_with_notes(vec![
        note(1, 0, 0, "whole file"),
        note(2, 10, 10, "last line"),
    ]);
    a.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert!(!on_card(&a), "home landed on the leading card");
    a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert!(!on_card(&a), "end landed on the trailing card");
}

#[test]
fn a_card_appearing_under_the_cursor_pushes_it_off() {
    let mut a = app_with_notes(vec![]);
    for _ in 0..3 {
        press(&mut a, 'j');
    }
    let before = a.cursor_line;
    // A note lands right where the cursor is sitting.
    a.store.threads = vec![note(1, 3, 3, "new note\nwith two lines")];
    let r = req("f.txt");
    a.apply_diff(&r, Ok(fake_diff(10)));
    draw(&mut a);
    assert!(!on_card(&a), "cursor was left inside the new card");
    assert!(a.cursor_line >= before);
}

#[test]
fn dragging_across_a_card_does_not_land_on_it() {
    let mut a = app_with_notes(vec![note(1, 2, 2, "one\ntwo\nthree")]);
    draw(&mut a);
    a.on_mouse(MouseEventKind::Down(MouseButton::Left), 1);
    for y in 2..9u16 {
        a.on_mouse(MouseEventKind::Drag(MouseButton::Left), y);
        assert!(!on_card(&a), "drag rested on a card at row {y}");
    }
}

#[test]
fn clicking_a_card_leaves_the_cursor_where_it_was() {
    let mut a = app_with_notes(vec![note(1, 2, 2, "note text")]);
    draw(&mut a);
    a.on_mouse(MouseEventKind::Down(MouseButton::Left), 1);
    let before = a.cursor_line;
    // Row 4 is inside the card (line 1 anchor, then the card's three rows).
    a.on_mouse(MouseEventKind::Down(MouseButton::Left), 4);
    assert_eq!(a.cursor_line, before, "a click on a card must be inert");
}

#[test]
fn annotating_a_range_with_no_source_lines_is_refused() {
    let mut a = app_with_notes(vec![note(1, 2, 2, "existing")]);
    draw(&mut a);
    // Force the selection onto the card itself, which no key or click can do
    // — this is the guard of last resort for the annotate path.
    let card = (0..a.doc.lines.len())
        .find(|i| {
            let t: String = a.doc.lines[*i]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            t.contains('╭')
        })
        .unwrap();
    a.cursor_line = card;
    a.select_anchor = Some(card);
    press(&mut a, 'a');
    assert!(a.popup_request.is_none(), "should not open the note popup");
    assert_eq!(
        a.active_flash(),
        Some("select added or context lines to annotate")
    );
}

#[test]
fn the_header_always_has_a_line_number_to_show() {
    let mut a = app_with_notes(vec![note(1, 3, 3, "x\ny\nz")]);
    for _ in 0..a.doc.lines.len() {
        press(&mut a, 'j');
        assert!(
            a.cursor_file_line().is_some(),
            "no source line at doc line {}",
            a.cursor_line
        );
    }
}

#[test]
fn a_selection_spanning_a_card_leaves_the_card_untinted() {
    let mut a = app_with_notes(vec![note(1, 3, 3, "one\ntwo")]);
    draw(&mut a);
    press(&mut a, 'v'); // start selecting at the top
    for _ in 0..6 {
        press(&mut a, 'j'); // extend down, across the card
    }
    let buf = draw(&mut a);

    let selected_bg = buf[(0, 1)].style().bg;
    let mut saw_card = false;
    for y in 1..H - 1 {
        let text = row(&buf, y);
        if text.contains('╭') || text.contains('│') || text.contains('╰') {
            saw_card = true;
            assert_ne!(
                buf[(0, y)].style().bg,
                selected_bg,
                "card row {y} is tinted as selected: {text:?}"
            );
        }
    }
    assert!(saw_card, "the card should be inside the selected range");
}

// ---- the inline composer ---------------------------------------------------

fn key(a: &mut PreviewApp, code: KeyCode, mods: KeyModifiers) {
    a.on_key(KeyEvent::new(code, mods));
}

fn type_text(a: &mut PreviewApp, text: &str) {
    for ch in text.chars() {
        key(a, KeyCode::Char(ch), KeyModifiers::NONE);
    }
}

fn body_lines(buf: &Buffer) -> Vec<String> {
    (1..H - 1).map(|y| row(buf, y)).collect()
}

/// An app showing a diff, with the cursor moved down `n` lines.
fn app_at(n: usize) -> PreviewApp {
    let mut a = app_with_notes(vec![]);
    for _ in 0..n {
        press(&mut a, 'j');
    }
    a
}

#[test]
fn annotate_opens_the_composer_inline_under_the_line() {
    let mut a = app_at(2);
    press(&mut a, 'a');
    assert!(a.composer.is_some(), "no composer");
    let buf = draw(&mut a);
    let body = body_lines(&buf);
    let top = body.iter().position(|l| l.contains('╭')).expect("no box");
    assert!(
        body[top].contains("new note · line 3"),
        "title: {:?}",
        body[top]
    );
    // ...directly under the line being commented on.
    assert!(
        body[top - 1].contains("line 2"),
        "anchor: {:?}",
        body[top - 1]
    );
}

#[test]
fn typing_in_the_composer_grows_the_box_and_saves_on_enter() {
    let mut a = app_at(1);
    press(&mut a, 'a');
    type_text(&mut a, "first");
    key(&mut a, KeyCode::Char('j'), KeyModifiers::CONTROL); // newline
    type_text(&mut a, "second");

    let body = body_lines(&draw(&mut a));
    assert!(body.iter().any(|l| l.contains("first")));
    assert!(body.iter().any(|l| l.contains("second")));

    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(a.composer.is_none(), "composer should close");
    assert_eq!(a.threads().len(), 1);
    assert_eq!(a.threads()[0].preview(), "first\nsecond");
    // The saved note's card stands where the composer was.
    let body = body_lines(&draw(&mut a));
    assert!(body.iter().any(|l| l.contains("note · line")));
}

#[test]
fn every_key_reaches_the_composer_instead_of_the_keymap() {
    let mut a = app_at(1);
    press(&mut a, 'a');
    // j/k/q/v/d would all be actions outside the composer.
    type_text(&mut a, "jkqvd note");
    assert_eq!(
        a.composer.as_ref().map(|c| c.input.text().to_string()),
        Some("jkqvd note".to_string())
    );
    assert!(!a.should_quit, "q must not quit while composing");
}

#[test]
fn esc_cancels_without_leaving_a_note() {
    let mut a = app_at(1);
    press(&mut a, 'a');
    type_text(&mut a, "never mind");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    assert!(a.composer.is_none());
    assert!(a.threads().is_empty());
    assert!(a.pending.is_none());
    let body = body_lines(&draw(&mut a));
    assert!(!body.iter().any(|l| l.contains('╭')), "box left behind");
}

#[test]
fn saving_an_empty_composer_is_a_cancel() {
    let mut a = app_at(1);
    press(&mut a, 'a');
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert!(a.threads().is_empty(), "an empty note is not worth sending");
    assert!(a.composer.is_none());
}

#[test]
fn the_composer_edits_an_existing_note_in_place() {
    let mut a = app_with_notes(vec![note(1, 3, 3, "original")]);
    assert!(a.begin_edit_note(1), "edit should open");
    assert_eq!(
        a.composer.as_ref().map(|c| c.input.text().to_string()),
        Some("original".to_string()),
        "prefilled with the note"
    );
    // The note's own card is hidden while its composer stands in for it.
    let body = body_lines(&draw(&mut a));
    assert_eq!(body.iter().filter(|l| l.contains('╭')).count(), 1);
    assert!(body.iter().any(|l| l.contains("edit note · line 3")));

    key(&mut a, KeyCode::Char('u'), KeyModifiers::CONTROL);
    type_text(&mut a, "rewritten");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(a.threads().len(), 1, "edited, not duplicated");
    assert_eq!(a.threads()[0].preview(), "rewritten");
}

#[test]
fn editing_a_note_in_another_file_asks_for_that_file_first() {
    let mut a = app_with_notes(vec![note(1, 3, 3, "elsewhere")]);
    a.store.threads[0].anchor.file = PathBuf::from("other.rs");
    // The preview knows which file the note belongs to, so it can ask for it
    // itself rather than refusing and making the list guess.
    assert!(
        matches!(a.show_for_note(1), Some(herdr_gitview::ipc::ToPreview::Show { file, .. }) if file == std::path::Path::new("other.rs"))
    );
    // An unknown id has nothing to show and nothing to edit.
    assert!(a.show_for_note(999).is_none());
    assert!(!a.begin_edit_note(999));
    // The shown file needs no re-Show.
    let mut a = app_with_notes(vec![note(1, 3, 3, "here")]);
    assert!(a.show_for_note(1).is_none());
    assert!(a.begin_edit_note(1));
}

#[test]
fn a_whole_file_note_composes_at_the_top() {
    let mut a = app_with_notes(vec![]);
    a.begin_file_note();
    let body = body_lines(&draw(&mut a));
    assert!(
        body[0].contains('╭'),
        "box should lead the diff: {:?}",
        body[0]
    );
    assert!(body[0].contains("new note · whole file"));

    type_text(&mut a, "about the file");
    key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(a.threads()[0].anchor.start, 0);
    assert_eq!(a.threads()[0].anchor.end, 0);
}

#[test]
fn the_cursor_cannot_walk_into_the_composer_either() {
    let mut a = app_at(2);
    press(&mut a, 'a');
    type_text(&mut a, "a\nb\nc");
    key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
    // After cancelling, walking the doc must still never touch a card.
    a.store.threads = vec![note(1, 3, 3, "x\ny")];
    let r = req("f.txt");
    a.apply_diff(&r, Ok(fake_diff(10)));
    draw(&mut a);
    for _ in 0..a.doc.lines.len() {
        press(&mut a, 'j');
        assert!(!on_card(&a));
    }
}

#[test]
fn shift_enter_and_friends_insert_a_newline_rather_than_saving() {
    for mods in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
    ] {
        let mut a = app_at(1);
        press(&mut a, 'a');
        type_text(&mut a, "one");
        key(&mut a, KeyCode::Enter, mods);
        type_text(&mut a, "two");
        assert!(a.composer.is_some(), "{mods:?} saved instead of newlining");
        assert_eq!(
            a.composer.as_ref().map(|c| c.input.text().to_string()),
            Some("one\ntwo".to_string()),
            "{mods:?}"
        );
    }
    // ...and ctrl+j, the one every terminal can deliver.
    let mut a = app_at(1);
    press(&mut a, 'a');
    type_text(&mut a, "one");
    key(&mut a, KeyCode::Char('j'), KeyModifiers::CONTROL);
    type_text(&mut a, "two");
    assert_eq!(
        a.composer.as_ref().map(|c| c.input.text().to_string()),
        Some("one\ntwo".to_string())
    );
}

#[test]
fn the_composer_moves_the_caret_like_a_text_area() {
    let mut a = app_at(1);
    press(&mut a, 'a');
    type_text(&mut a, "helo world");
    // back to just after "hel" and fix the typo
    for _ in 0..7 {
        key(&mut a, KeyCode::Left, KeyModifiers::NONE);
    }
    type_text(&mut a, "l");
    assert_eq!(
        a.composer.as_ref().map(|c| c.input.text().to_string()),
        Some("hello world".to_string())
    );
    // home/end and word delete
    key(&mut a, KeyCode::End, KeyModifiers::NONE);
    key(&mut a, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(
        a.composer.as_ref().map(|c| c.input.text().to_string()),
        Some("hello ".to_string())
    );
}

#[test]
fn an_empty_composer_shows_what_it_wants() {
    let mut a = app_at(1);
    press(&mut a, 'a');
    let body = body_lines(&draw(&mut a));
    assert!(
        body.iter().any(|l| l.contains("write a note")),
        "no placeholder: {body:?}"
    );
    // ...which disappears as soon as you type.
    type_text(&mut a, "x");
    let body = body_lines(&draw(&mut a));
    assert!(!body.iter().any(|l| l.contains("write a note")));
}

#[test]
fn cards_stay_readable_on_a_wide_pane_and_clear_the_right_edge() {
    let mut a = app_with_notes(vec![note(1, 3, 3, &"word ".repeat(60))]);
    for width in [50u16, 80, 120, 200] {
        let mut term = Terminal::new(TestBackend::new(width, H)).unwrap();
        term.draw(|f| ui::render(f, &mut a)).unwrap();
        let buf = term.backend().buffer().clone();
        let top = (1..H - 1)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .find(|l| l.contains('╭'))
            .expect("no card");
        let box_w = top.trim_end().width() - top.find('╭').unwrap();
        assert!(box_w <= 60, "width {width}: box is {box_w} cols");
        assert!(
            top.trim_end().width() < width as usize,
            "width {width}: card touches the right edge"
        );
    }
}

// ---- regressions found by review -------------------------------------------

#[test]
fn focus_note_finds_the_right_card_while_another_is_being_edited() {
    // `card_starts` used to be rank-indexed over the notes *minus* the one
    // being edited, while `scroll_to_note` computed the rank over *all* of
    // them — so hovering a note in the notes view scrolled to a different one.
    let mut a = app_with_notes(vec![
        note(1, 2, 2, "note one"),
        note(2, 20, 20, "note two"),
        note(3, 40, 40, "note three"),
    ]);
    let r = req("f.txt");
    a.apply_diff(&r, Ok(fake_diff(60)));
    draw(&mut a);

    assert!(a.begin_edit_note(1), "open the composer on note 1");
    a.focus_note(2);
    let buf = draw(&mut a);
    let card = (1..H - 1)
        .map(|y| row(&buf, y))
        .find(|l| l.contains("note ·"))
        .unwrap_or_default();
    assert!(
        card.contains("line 20"),
        "scrolled to the wrong note: {card:?}"
    );
}

#[test]
fn the_composer_survives_a_diff_larger_than_the_render_cap() {
    // Cards used to be spliced in and *then* the doc truncated, so past the
    // cap the composer was deleted from the doc while still owning the
    // keyboard: you typed blind into a box that wasn't drawn.
    let mut a = app();
    let r = req("f.txt");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(20_050)));
    draw(&mut a);

    a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    press(&mut a, 'a');
    type_text(&mut a, "still visible");
    let body = body_lines(&draw(&mut a));
    assert!(
        body.iter().any(|l| l.contains("still visible")),
        "composer not on screen: {body:?}"
    );
    assert!(body.iter().any(|l| l.contains('╭')));
}

#[test]
fn the_truncation_notice_is_chrome_not_a_source_line() {
    let mut a = app();
    let r = req("f.txt");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(20_050)));
    draw(&mut a);

    a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    let text: String = a.doc.lines[a.cursor_line]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !text.contains("truncated"),
        "the cursor rested on the notice and reported {:?}",
        a.cursor_file_line()
    );
    assert!(a.cursor_file_line().is_some());
}

#[test]
fn a_note_whose_line_is_gone_says_so_instead_of_posing_as_a_file_note() {
    // `line_for_new(...).unwrap_or(0)` conflated "whole file" with "not
    // found", so a note left behind by a shrinking diff silently became a
    // whole-file note pinned at the top.
    let mut a = app_with_notes(vec![note(1, 900, 999, "orphaned")]);
    let r = req("f.txt");
    a.apply_diff(&r, Ok(fake_diff(10)));
    let body = body_lines(&draw(&mut a));
    let top = body.iter().position(|l| l.contains('╭')).expect("no card");
    assert!(
        body[top].contains("anchor lost"),
        "orphaned note not flagged: {:?}",
        body[top]
    );
    assert!(
        body[top].contains("900-999"),
        "should still name its lines: {:?}",
        body[top]
    );

    // A real whole-file note is not flagged.
    let mut a = app_with_notes(vec![note(1, 0, 0, "about the file")]);
    let r = req("f.txt");
    a.apply_diff(&r, Ok(fake_diff(10)));
    let body = body_lines(&draw(&mut a));
    let top = body.iter().position(|l| l.contains('╭')).unwrap();
    assert!(body[top].contains("whole file"));
    assert!(!body[top].contains("anchor lost"));
}

// ---- persistence -----------------------------------------------------------

/// The headline claim of the threads fork: a conversation outlives the pane.
/// Closing gitview and reopening it must bring the review back, ids intact.
#[test]
fn sending_marks_only_the_thread_that_was_actually_asked() {
    use herdr_gitview::thread::ThreadState;
    let mut a = app_with_notes(vec![note(1, 2, 2, "first"), note(2, 5, 5, "second")]);
    let agent = herdr_gitview::thread::AgentRef {
        pane: "w1:p1".into(),
        agent: "claude".into(),
        session: None,
        session_kind: None,
    };
    a.mark_turns_sent(1, &[0], &agent);

    assert_eq!(a.threads()[0].state, ThreadState::Sent);
    assert!(!a.threads()[0].has_unsent());
    assert_eq!(
        a.threads()[1].state,
        ThreadState::Draft,
        "thread 2 was never asked — one turn succeeding says nothing about another"
    );
    assert!(a.threads()[1].has_unsent());
}

/// A failure that provably never reached the agent must not touch the `sent`
/// marks: the thread was never moved to Sent, and clearing them would also
/// unmark turns an *earlier*, successful send delivered — re-asking them.
#[test]
fn an_undelivered_failure_leaves_earlier_delivered_turns_alone() {
    use herdr_gitview::thread::{Author, ThreadState};
    let mut a = app_with_notes(vec![note(1, 2, 2, "first question")]);
    let agent = herdr_gitview::thread::AgentRef {
        pane: "w1:p1".into(),
        agent: "claude".into(),
        session: None,
        session_kind: None,
    };
    // Round one succeeds and is answered.
    a.mark_turns_sent(1, &[0], &agent);
    a.append_reply(1, "an answer".into());
    // The user follows up; round two never reaches the agent.
    a.append_human_turn(1, "follow-up".into());
    a.mark_thread_failed(1, &[2], "the agent is blocked", true);

    let t = &a.threads()[0];
    assert_eq!(
        t.unsent().count(),
        1,
        "only the follow-up is pending, not the already-answered question"
    );
    assert_eq!(
        t.unsent().next().unwrap().text,
        "follow-up",
        "the original question must not be re-asked"
    );
    assert_ne!(t.state, ThreadState::Failed, "nothing was delivered");
    assert_eq!(
        t.last().unwrap().author,
        Author::System,
        "the failure is recorded in the thread, not just flashed"
    );
}

/// Regression: a request can wait in the worker's queue behind a multi-minute
/// turn, and the thread can gain a turn while it waits. Delivery must mark
/// only the turns the prompt was actually composed from — marking "every
/// Human turn" retires text the agent was never asked about.
#[test]
fn delivery_marks_only_the_turns_that_were_asked_about() {
    let mut a = app_with_notes(vec![note(1, 2, 2, "first question")]);
    let agent = herdr_gitview::thread::AgentRef {
        pane: "w1:p1".into(),
        agent: "claude".into(),
        session: None,
        session_kind: None,
    };
    // Queued with turn 0 only; the user then adds a second question.
    let asked = a.threads()[0].unsent_indices();
    assert_eq!(asked, vec![0]);
    a.append_human_turn(1, "second question".into());

    a.mark_turns_sent(1, &asked, &agent);

    let t = &a.threads()[0];
    assert_eq!(
        t.unsent().count(),
        1,
        "the turn added after queuing is still pending"
    );
    assert_eq!(t.unsent().next().unwrap().text, "second question");
}

/// Regression: `Delivered` is optimistic — it fires before herdr has accepted
/// the prompt. A refusal arriving after it must hand back exactly the turns
/// that attempt claimed, or the thread is stranded as sent-but-never-asked.
#[test]
fn a_refusal_after_optimistic_delivery_hands_the_question_back() {
    use herdr_gitview::thread::ThreadState;
    let mut a = app_with_notes(vec![note(1, 2, 2, "please fix this")]);
    let agent = herdr_gitview::thread::AgentRef {
        pane: "w1:p1".into(),
        agent: "claude".into(),
        session: None,
        session_kind: None,
    };
    a.mark_turns_sent(1, &[0], &agent);
    assert!(!a.threads()[0].has_unsent(), "optimistically marked sent");

    // herdr then rejects it: the agent was blocked. Nothing was delivered.
    a.mark_thread_failed(1, &[0], "the agent is blocked", true);

    let t = &a.threads()[0];
    assert!(
        t.has_unsent(),
        "the question is pending again and can be re-sent"
    );
    assert_eq!(t.state, ThreadState::Draft);
    assert_ne!(t.state, ThreadState::Sent, "not stranded as sent");
}

/// A failure that may have reached the agent is durable: a flash lasts three
/// seconds, and after it the thread would be indistinguishable from one the
/// agent is still working on.
#[test]
fn a_possibly_delivered_failure_is_recorded_on_the_thread() {
    use herdr_gitview::thread::{Author, ThreadState};
    let mut a = app_with_notes(vec![note(1, 2, 2, "question")]);
    a.mark_turns_sent(
        1,
        &[0],
        &herdr_gitview::thread::AgentRef {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            session: None,
            session_kind: None,
        },
    );
    a.mark_thread_failed(1, &[0], "timed out waiting for the agent", false);

    let t = &a.threads()[0];
    assert_eq!(t.state, ThreadState::Failed);
    assert_eq!(t.last().unwrap().author, Author::System);
    assert!(
        t.last().unwrap().text.contains("timed out"),
        "the reason survives the flash"
    );
}

#[test]
fn threads_survive_a_pane_restart() {
    let dir = std::env::temp_dir().join(format!("gitview-persist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("threads.json");

    let mk = || {
        let cfg = Config::default();
        let keys = Keymap::build(&HashMap::new()).unwrap();
        let repo = Repo {
            root: PathBuf::from("."),
        };
        PreviewApp::new(cfg, repo, keys, path.clone())
    };

    // Session one: ask a question, then get an answer.
    let mut a = mk();
    let r = req("f.txt");
    a.begin_show(r.clone());
    a.apply_diff(&r, Ok(fake_diff(10)));
    let id = a.store.alloc_id();
    a.store.threads.push(note(id, 3, 3, "why is this here?"));
    // `append_reply` is a real mutation, so it persists — no test-only hook.
    a.append_reply(id, "because of the retry path.".into());
    assert_eq!(a.threads()[0].turns.len(), 2);

    // Session two: a brand-new app over the same store.
    let b = mk();
    assert_eq!(b.threads().len(), 1, "the thread came back");
    let t = &b.threads()[0];
    assert_eq!(
        t.id, id,
        "and kept its id, so list commands still address it"
    );
    assert_eq!(t.turns.len(), 2, "with both sides of the conversation");
    assert_eq!(t.turns[0].text, "why is this here?");
    assert_eq!(t.turns[1].text, "because of the retry path.");
    assert_eq!(t.state, herdr_gitview::thread::ThreadState::Answered);
    assert_eq!(
        t.anchor.file,
        PathBuf::from("f.txt"),
        "anchored where it was written"
    );

    // And a new thread must not reuse the restored id — an id collision
    // silently retargets every list→preview command at the wrong thread.
    let mut c = mk();
    assert!(
        c.store.alloc_id() > id,
        "a fresh id must clear the restored id {id}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
