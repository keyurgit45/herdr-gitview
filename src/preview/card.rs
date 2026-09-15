//! Note cards: the boxed blocks spliced into the diff under the lines they
//! comment on, and the composer box that stands in their place while a note
//! is being written.
//!
//! Pure presentation — nothing here touches `PreviewApp`. It sits beside
//! `render.rs` rather than in `ui.rs` because these lines are spliced into
//! the *document* before any frame is drawn.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::palette;
use crate::textarea::TextArea;

/// Floor for the card width, so a card still has a shape before the first
/// draw has reported the real pane width.
pub const MIN_WIDTH: u16 = 24;

/// What a spliced block is. An enum rather than `Option<u64>`: once the
/// composer can be nested inside a thread it also has a thread id, and an
/// `Option` would quietly classify it as a thread card and leave
/// `composer_span` unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    /// A saved thread's card.
    Thread(u64),
    /// The standalone composer box.
    Composer,
}

/// One block spliced into the diff: a thread's card, or the composer.
pub struct Card {
    /// Doc line (pre-splice) the block is inserted at.
    pub anchor: usize,
    pub lines: Vec<Line<'static>>,
    pub kind: CardKind,
}

/// Where a note anchors in the built diff, and whether its line is gone.
/// `end == 0` means a whole-file note, which legitimately sits at the top;
/// a line that cannot be found is a *different* thing and says so.
pub fn anchor_of(built: &super::render::DiffDoc, end: u32) -> (usize, bool) {
    if end == 0 {
        return (0, false);
    }
    match built.line_for_new(end) {
        Some(line) => (line + 1, false),
        None => (0, true),
    }
}

/// Styles for the pieces of a card title, so the badge does not inherit the
/// title colour. Every span handed to `card_box_titled` MUST carry a
/// non-default style: the body loop rewrites default-styled spans to the body
/// colour, silently flattening anything that forgot.
pub fn title_style() -> Style {
    Style::new()
        .fg(palette::ACCENT)
        .add_modifier(Modifier::BOLD)
}

pub fn badge_style(state: crate::thread::ThreadState) -> Style {
    use crate::thread::ThreadState as S;
    let c = match state {
        S::Draft => Color::Gray,
        S::Sent => palette::INFO,
        S::Answered => palette::OK,
        S::Failed => palette::BAD,
        S::Resolved => Color::DarkGray,
    };
    Style::new().fg(c)
}

pub fn dim_style() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// `<prefix> · line 12` / `· lines 12-20` / `· whole file`.
pub fn range_label(prefix: &str, start: u32, end: u32) -> String {
    match (start, end) {
        (_, 0) => format!("{prefix} · whole file"),
        (s, e) if s == e => format!("{prefix} · line {s}"),
        (s, e) => format!("{prefix} · lines {s}-{e}"),
    }
}

/// Recolor the line-number cell of a commented row so it reads as annotated
/// even when its card is off-screen. The number is the first span on a
/// context row and the second on a `+`/`-` row (whose first span is the
/// change bar), which the row's old/new numbers identify.
pub fn accent_gutter(lines: &mut [Line<'static>], idx: usize, built: &super::render::DiffDoc) {
    let Some((old, new)) = built.numbers_of_line(idx) else {
        return; // a fold row has no line number to accent
    };
    let span_idx = match (old, new) {
        (Some(_), Some(_)) => 0, // context: " 1234 "
        _ => 1,                  // insertion/deletion: "▌" then "1234 "
    };
    if let Some(span) = lines.get_mut(idx).and_then(|l| l.spans.get_mut(span_idx)) {
        span.style = span.style.fg(palette::ACCENT).add_modifier(Modifier::BOLD);
    }
}

/// Your side of a conversation as one boxed block:
///
/// ```text
///   ╭─ note · lines 12-20 · 2 turns · replied ──╮
///   │ why is this unwrap safe?                  │
///   ╰───────────────────────────────────────────╯
/// ```
///
/// Only Human turns are drawn. The agent's reply is captured and persisted,
/// and the title's `replied` badge says it arrived — but it is read in the
/// agent's own pane rather than here, because an answer several paragraphs
/// long would bury the code the card is attached to. The turn count in the
/// title still counts the whole conversation.
///
/// Collapsed, it shows only your newest note plus a count of what is hidden,
/// which matters once a thread has follow-ups.
pub fn thread_card(
    title: Vec<Span<'static>>,
    thread: &crate::thread::Thread,
    width: usize,
    theme: crate::config::Theme,
    collapsed: bool,
) -> Vec<Line<'static>> {
    let text_w = card_text_width(width);

    // Only your own turns are drawn. An agent's reply routinely runs to
    // several paragraphs, and rendering it inline buries the very code the
    // card is commenting on — while the answer is already on screen in the
    // agent's own pane. The reply is still captured, still persisted, and
    // still drives the "replied" badge in this card's title; the card simply
    // does not repeat it.
    //
    // Because every drawn turn is therefore yours, there are no role labels
    // and no indent: a blank line is enough to separate one from the next.
    let mine: Vec<&crate::thread::Turn> = thread
        .turns
        .iter()
        .filter(|t| t.author == crate::thread::Author::Human)
        .collect();

    let shown: Vec<&crate::thread::Turn> = if collapsed {
        mine.iter().rev().take(1).copied().collect()
    } else {
        mine.clone()
    };
    let hidden = mine.len() - shown.len();

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    if hidden > 0 {
        rows.push(vec![Span::styled(
            format!(
                "… {hidden} earlier note{}",
                if hidden == 1 { "" } else { "s" }
            ),
            dim_style(),
        )]);
    }
    for (i, turn) in shown.iter().enumerate() {
        // A blank line between notes, never before the first one.
        if i > 0 || hidden > 0 {
            rows.push(vec![Span::raw(String::new())]);
        }
        for logical in turn.text.split('\n') {
            for piece in crate::textarea::wrap_plain(logical, text_w) {
                rows.push(vec![Span::raw(piece)]);
            }
        }
    }
    card_box_titled(title, rows, width, theme, false)
}

/// The text width inside a card box at pane `width`: the indent, the two
/// borders, and the space either side of the text.
pub fn card_text_width(width: usize) -> usize {
    card_box_width(width).saturating_sub(4).max(1)
}

fn card_box_width(width: usize) -> usize {
    // Never wider than the pane allows, never narrower than a usable box.
    let outer = width.saturating_sub(CARD_INDENT).max(MIN_WIDTH as usize);
    width
        .saturating_sub(CARD_INDENT + CARD_RIGHT_MARGIN)
        .min(MAX_CARD_WIDTH)
        .max(MIN_WIDTH as usize)
        .min(outer)
}

/// Indent of every card from the left edge, so a card reads as a comment
/// *on* the code rather than another diff row.
const CARD_INDENT: usize = 4;

/// Air left to the right of a card, so it doesn't run into the pane edge.
const CARD_RIGHT_MARGIN: usize = 6;

/// Cards stop growing past this: a comment is prose, and prose set across a
/// very wide pane is hard to read (and hard to tell apart from the diff).
const MAX_CARD_WIDTH: usize = 60;

/// The open composer as a card, with the caret drawn in place and an accented
/// border so it is obviously the thing taking your keystrokes.
pub fn composer_card(
    label: &str,
    input: &TextArea,
    width: usize,
    theme: crate::config::Theme,
) -> Vec<Line<'static>> {
    let text_w = card_text_width(width);
    let caret = Style::new().add_modifier(Modifier::REVERSED);
    // An empty box says what it wants, with the caret waiting in front of it.
    if input.is_empty() {
        let hint = crate::textarea::elide_tail("write a note…", text_w.saturating_sub(1));
        return card_box(
            label,
            vec![vec![
                Span::styled(" ", caret),
                Span::styled(hint, Style::new().add_modifier(Modifier::DIM)),
            ]],
            width,
            theme,
            true,
        );
    }
    let rows = input.layout(text_w);
    let caret_row = input.caret_row(&rows);
    let body: Vec<Vec<Span<'static>>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let text = &input.text()[r.clone()];
            if i != caret_row {
                return vec![Span::raw(text.to_string())];
            }
            let at = input.caret() - r.start;
            let (before, rest) = text.split_at(at);
            let mut chars = rest.chars();
            let under = chars.next();
            vec![
                Span::raw(before.to_string()),
                Span::styled(under.map(String::from).unwrap_or_else(|| " ".into()), caret),
                Span::raw(chars.collect::<String>()),
            ]
        })
        .collect();
    card_box(label, body, width, theme, true)
}

/// Fit a multi-span title into `avail` columns.
///
/// `spans[0]` is the range label; everything after it is a suffix badge, in
/// increasing order of importance ("· 2 turns", then the state, then
/// "· anchor lost"). A narrow pane elides the *label* and drops the least
/// important badges, rather than truncating the tail — a card whose anchor is
/// gone has to be able to say so at 60 columns, which is the width the
/// scenario tests run at.
fn elide_title(mut spans: Vec<Span<'static>>, avail: usize) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthStr;
    /// Wide enough for "note · lines 120-140 ". Below this the label starts
    /// losing its line numbers, which is what identifies the thread — and
    /// which nothing else on the card can tell you once the anchor is lost.
    /// A badge is dropped instead.
    const MIN_LABEL: usize = 21;

    let total = |s: &[Span<'static>]| -> usize { s.iter().map(|x| x.content.width()).sum() };
    if spans.is_empty() || total(&spans) <= avail {
        return spans;
    }
    while spans.len() > 1 {
        let suffixes = total(&spans) - spans[0].content.width();
        // Stop as soon as the label has room, OR as soon as everything fits
        // outright — testing only the MIN_LABEL floor kept dropping badges
        // that would have fitted once an earlier one was gone.
        if total(&spans) <= avail || avail.saturating_sub(suffixes) >= MIN_LABEL {
            break;
        }
        spans.remove(1); // least important badge still present
    }
    let suffixes = total(&spans) - spans[0].content.width();
    let room = avail.saturating_sub(suffixes);
    let head = crate::textarea::elide_tail(&spans[0].content, room);
    spans[0] = Span::styled(head, spans[0].style);
    spans
}

/// Draw a box titled with a single elided string.
fn card_box(
    label: &str,
    rows: Vec<Vec<Span<'static>>>,
    width: usize,
    theme: crate::config::Theme,
    accent: bool,
) -> Vec<Line<'static>> {
    let box_w = card_box_width(width);
    let label = format!(" {label} ");
    let label = crate::textarea::elide_tail(&label, box_w.saturating_sub(3));
    card_box_titled(
        vec![Span::styled(label, title_style())],
        rows,
        width,
        theme,
        accent,
    )
}

/// Draw a titled box around pre-wrapped rows of spans, where the title is
/// itself a span list. `fill` is computed from the summed span widths, so a
/// multi-coloured title (label + turn count + state badge) still closes its
/// border in the right column.
pub fn card_box_titled(
    title: Vec<Span<'static>>,
    rows: Vec<Vec<Span<'static>>>,
    width: usize,
    theme: crate::config::Theme,
    accent: bool,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    const INDENT: usize = CARD_INDENT;
    let box_w = card_box_width(width);
    let text_w = card_text_width(width);
    let border = Style::new().fg(if accent {
        palette::ACCENT
    } else if theme.is_light() {
        Color::Rgb(0x9a, 0xa0, 0xa6)
    } else {
        Color::Rgb(0x6c, 0x70, 0x86)
    });
    let body = Style::new().fg(if theme.is_light() {
        Color::Rgb(0x4c, 0x4f, 0x69)
    } else {
        Color::Rgb(0xcd, 0xd6, 0xf4)
    });
    let pad = || Span::raw(" ".repeat(INDENT));

    // Elide across the span list rather than per span, so a long label cannot
    // push the state badge out of the title: the badge is the part that says
    // whether the agent has the ball, so it is the last thing to drop.
    let avail = box_w.saturating_sub(3);
    let title = elide_title(title, avail);
    let used: usize = title.iter().map(|s| s.content.width()).sum();
    let fill = avail.saturating_sub(used);
    let mut head = vec![pad(), Span::styled("╭─", border)];
    head.extend(title);
    head.push(Span::styled(format!("{}╮", "─".repeat(fill)), border));
    let mut lines = vec![Line::from(head)];

    // An empty note still gets one body row, so the box never collapses.
    let rows = if rows.is_empty() {
        vec![vec![Span::raw(String::new())]]
    } else {
        rows
    };
    for spans in rows {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let gap = " ".repeat(text_w.saturating_sub(used));
        let mut line = vec![pad(), Span::styled("│ ", border)];
        line.extend(spans.into_iter().map(|s| {
            if s.style == Style::default() {
                Span::styled(s.content, body)
            } else {
                s
            }
        }));
        line.push(Span::styled(format!("{gap} │"), border));
        lines.push(Line::from(line));
    }

    lines.push(Line::from(vec![
        pad(),
        Span::styled(format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))), border),
    ]));
    lines
}
