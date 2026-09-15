//! Semantic UI colours as explicit RGB, deliberately bypassing the terminal's
//! ANSI palette.
//!
//! A stylised terminal theme is free to put anything in the ANSI slots, and
//! plenty do. Ghostty's "Black Metal" maps ANSI red to a teal `#486e6f` and
//! ANSI green to a dusty pink `#dd9999`, with blue, magenta and cyan all
//! collapsed to identical greys. Under it, every `Color::Green` in this UI
//! read as pink and every `Color::Red` as teal — so an answered thread and a
//! failed one swapped signals, and the discard-confirm dialog painted its
//! "yes" pink and its "no" teal.
//!
//! Diff tints never had this problem; they were always `Color::Rgb`. These are
//! the status signals that were not.
//!
//! The values are muted rather than vivid so they still sit well in a dark,
//! low-contrast terminal, while staying unambiguous as good/bad. They are
//! single values rather than per-theme pairs: each is a mid-tone chosen to
//! stay legible against both a black and a white background.

use ratatui::style::Color;

/// Good: an added line, an answered thread, an idle agent.
pub const OK: Color = Color::Rgb(0x5e, 0x9c, 0x63);

/// Bad: a removed line, a failed send, a system error, a destructive action.
pub const BAD: Color = Color::Rgb(0xb3, 0x5a, 0x5d);

/// Attention: titles, the focused card, your own turns.
pub const ACCENT: Color = Color::Rgb(0xb8, 0x94, 0x5a);

/// In progress: a sent thread, a working agent, section headers.
pub const INFO: Color = Color::Rgb(0x6a, 0x8f, 0xb5);
