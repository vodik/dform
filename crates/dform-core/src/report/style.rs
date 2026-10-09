//! How the report's text is painted: plain, or with the terminal's colours for
//! what a piece of the plan is (`Paint`), a change's by its kind.

use super::Why;
use super::labels::marker_of;
use super::mask::Shown;
use crate::provider::ActionKind;

/// How the report's text is painted: plain (what `text` returns, every
/// golden, `--json` and the plan file never see colour), or ANSI colour by
/// the plan's own semantics (`--color`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

/// What a piece of the plan is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// `+`: green.
    Create,
    /// `~`: yellow.
    Update,
    /// `-`: red.
    Delete,
    /// `-/+` and `+/-`: magenta.
    Replace,
    /// A `?` null: cyan.
    Null,
    /// A note about a value where a value would be, `(sensitive)`,
    /// `(bootstrap): kept`: dim, colour being for the marks.
    Note,
    /// Conflicts and denies: red.
    Error,
    /// The pending-group line: the warning colour, bold yellow.
    Warn,
    /// Addresses, section headers, witness names: bold.
    Bold,
    /// The site column: dim (R-111).
    Dim,
    /// `because`: cyan.
    Because,
    /// `held for approval`: magenta.
    Held,
}

impl Style {
    pub const PLAIN: Style = Style { color: false };

    /// `s` in `p`'s colour; unchanged when plain.
    pub fn paint(&self, p: Paint, s: &str) -> String {
        if !self.color || s.is_empty() {
            return s.to_string();
        }
        let sgr = match p {
            Paint::Create => "32",
            Paint::Update => "33",
            Paint::Delete | Paint::Error => "31",
            Paint::Replace => "35",
            Paint::Null => "36",
            Paint::Note => "2",
            Paint::Warn => "33",
            Paint::Bold => "1",
            Paint::Dim => "2",
            Paint::Because => "36",
            Paint::Held => "35",
        };
        format!("\x1b[{sgr}m{s}\x1b[0m")
    }

    /// An action's marker in its kind's colour.
    pub(super) fn marker(&self, k: &ActionKind) -> String {
        match kind_paint(k) {
            Some(p) => self.paint(p, marker_of(k)),
            None => marker_of(k).to_string(),
        }
    }

    /// A change's address, bold in its kind's colour (R-111).
    pub(super) fn address(&self, k: &ActionKind, s: &str) -> String {
        if !self.color || s.is_empty() {
            return s.to_string();
        }
        let sgr = match kind_paint(k) {
            Some(Paint::Create) => "1;32",
            Some(Paint::Update) => "1;33",
            Some(Paint::Delete) => "1;31",
            Some(_) => "1;35",
            None => "1",
        };
        format!("\x1b[{sgr}m{s}\x1b[0m")
    }

    /// A note about a value, printed where a value would be
    /// (`(sensitive)`, [`KEPT`](crate::report::KEPT)): dim, so it does not read as one.
    pub fn note(&self, s: &str) -> String {
        self.paint(Paint::Note, s)
    }

    /// A value laid out as text (`{ "k": (sensitive), .. }`) with each
    /// `(sensitive..)` outside a string painted as a [`Style::note`].
    pub(super) fn notes_in(&self, text: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        let mut out = String::new();
        let (mut quoted, mut escaped) = (false, false);
        let mut rest = text;
        while let Some(c) = rest.chars().next() {
            if !quoted
                && rest.starts_with("(sensitive")
                && let Some(end) = rest.find(')')
            {
                out.push_str(&self.note(&rest[..=end]));
                rest = &rest[end + 1..];
                continue;
            }
            match c {
                '\\' if quoted && !escaped => escaped = true,
                '"' if !escaped => {
                    quoted = !quoted;
                    escaped = false
                }
                _ => escaped = false,
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// A value as the plan prints it at `why`: `(sensitive)` a note.
    pub(super) fn said(&self, v: &Shown, why: Why) -> String {
        match v {
            Shown::Sensitive(_) => self.note(&v.said(why)),
            _ => v.said(why),
        }
    }

    /// One side of a change: a null cyan, a sensitive value a note.
    pub(super) fn shown(&self, v: &Shown) -> String {
        match v {
            Shown::Null { .. } => self.paint(Paint::Null, &v.text()),
            Shown::Sensitive(_) => self.note(&v.text()),
            _ => v.text(),
        }
    }
}

/// The colour of a change of kind `k`: `+` green, `~` yellow, `±`
/// magenta, `-` red.
pub(super) fn kind_paint(k: &ActionKind) -> Option<Paint> {
    match k {
        ActionKind::Create | ActionKind::Adopt => Some(Paint::Create),
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending | ActionKind::Forget => {
            Some(Paint::Update)
        }
        ActionKind::Delete | ActionKind::DeleteDeposed => Some(Paint::Delete),
        ActionKind::Replace { .. } => Some(Paint::Replace),
        ActionKind::Noop => None,
    }
}
