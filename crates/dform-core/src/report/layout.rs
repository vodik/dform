//! The page the report prints on (R-111): its rows, each a left column and what
//! may follow it, and the right column aligned across them and folded at the page's
//! width.

use super::style::{Paint, Style};

/// The page width the right column folds at.
pub const WIDTH: usize = 100;

/// The right column starts here, unless every left column is narrower.
pub(super) const COLUMN: usize = 52;

/// One printed line: its text, its visible width, and what may follow it
/// in the right column, the longest that fits first.
pub(super) struct Row {
    pub(super) left: String,
    pub(super) width: usize,
    pub(super) right: Vec<String>,
    /// The right column is what the line says (a deny's wait): when none
    /// fits beside it, the shortest goes on the line below.
    pub(super) keep: bool,
    /// The right column is said, not a note: unpainted (an apply's
    /// status once its call answered, R-206), where a site is dim.
    pub(super) set: bool,
    /// The right column starts after it also while it has none: a line
    /// whose right column comes and goes (an apply's change) moves no
    /// other line's.
    pub(super) aligned: bool,
}

impl Row {
    pub(super) fn new(plain: &str, painted: String) -> Row {
        Row {
            left: painted,
            width: plain.chars().count(),
            right: Vec::new(),
            keep: false,
            set: false,
            aligned: false,
        }
    }

    pub(super) fn plain(s: String) -> Row {
        Row {
            width: s.chars().count(),
            left: s,
            right: Vec::new(),
            keep: false,
            set: false,
            aligned: false,
        }
    }

    pub(super) fn with(mut self, right: Vec<String>) -> Row {
        self.right = right.into_iter().filter(|r| !r.is_empty()).collect();
        self
    }

    /// The right column is never folded to nothing.
    pub(super) fn kept(mut self) -> Row {
        self.keep = true;
        self
    }

    /// The right column unpainted.
    pub(super) fn set(mut self) -> Row {
        self.set = true;
        self
    }

    /// The right column after it, whether it has one now or not.
    pub(super) fn aligned(mut self) -> Row {
        self.aligned = true;
        self
    }
}

/// The rows, the right column aligned across them and dim (R-111); a
/// right column that does not fit in [`WIDTH`] folds to a shorter one, or
/// to nothing; a kept one to the line below.
pub(super) fn layout(rows: &[Row], style: Style) -> String {
    let col = rows
        .iter()
        .filter(|r| (r.aligned || !r.right.is_empty()) && r.width + 2 <= COLUMN)
        .map(|r| r.width + 2)
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for r in rows {
        out.push_str(&r.left);
        let at = col.max(r.width + 2);
        if let Some(x) = r.right.iter().find(|x| at + x.chars().count() <= WIDTH) {
            out.push_str(&" ".repeat(at - r.width));
            match r.set {
                true => out.push_str(x),
                false => out.push_str(&style.paint(Paint::Dim, x)),
            }
        } else if let Some(x) = r.right.last().filter(|_| r.keep) {
            let indent = r.left.len() - r.left.trim_start().len() + 4;
            out.push('\n');
            out.push_str(&" ".repeat(indent));
            out.push_str(&style.paint(Paint::Dim, x));
        }
        out.push('\n');
    }
    out
}
