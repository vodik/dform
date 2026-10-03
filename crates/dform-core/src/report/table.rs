//! The result-set printer (R-63): what `query`, `output`, `test` and the
//! listings (`stack list`, `state show`, `dev strata`, `dev effects`)
//! print, as a terse SQL client would. A header line, then one line per
//! row, columns aligned with two-space gutters and no borders; `(N rows)`
//! under a table past five rows (and under an empty one). Values in
//! surface spelling (R-28, `Redactor::cell`): a secret as `secret(SIZE)`,
//! a null as its `?T["A"].p` label. A string longer than a screen is
//! folded in its cell: its first line and `.. (SIZE, N lines)`. `--json`
//! is an array of objects keyed by column.
//!
//! Colour (R-14) through the report's [`Style`]: the header bold, a null
//! cyan, a secret dim. The width a cell folds at is the terminal's (100
//! when the output is not one).

use super::{Paint, Style};
use crate::query::{Redactor, size};
use crate::value::Value;
use serde_json::Value as Json;

/// Lines past which a string is folded, whatever its width.
const SCREEN_LINES: usize = 24;

/// The width of a folded string's first line, before its `..`.
const FOLDED: usize = 40;

/// How a table is rendered: the width a string folds at, and the colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub width: usize,
    pub style: Style,
}

impl Options {
    /// What a table renders as when it is not to a terminal: 100 columns,
    /// no colour. Every golden is in it.
    pub const PLAIN: Options = Options {
        width: 100,
        style: Style::PLAIN,
    };
}

impl Default for Options {
    fn default() -> Options {
        Options::PLAIN
    }
}

/// One cell: its text, how it is painted, its `--json`, and a string
/// value's own text (what folding reads).
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    text: String,
    paint: Option<Paint>,
    json: Json,
    string: Option<String>,
}

impl Cell {
    /// Text as it is (a name, a path, a kind), a JSON string.
    pub fn text(s: impl Into<String>) -> Cell {
        let text = s.into();
        Cell {
            json: Json::String(text.clone()),
            text,
            paint: None,
            string: None,
        }
    }

    /// A value in surface spelling, redacted: a secret is `secret(SIZE)`
    /// (its JSON the redactor's `{"sensitive": label}`), a null its label.
    pub fn value(v: &Value, r: &Redactor) -> Cell {
        let secret = r.is_secret(v);
        Cell {
            text: r.cell(v),
            paint: if secret {
                Some(Paint::Sensitive)
            } else if matches!(v, Value::Null { .. }) {
                Some(Paint::Null)
            } else {
                None
            },
            json: r.json(v),
            string: match v {
                Value::Str(s) if !secret => Some(s.clone()),
                _ => None,
            },
        }
    }

    /// A secret known by its size, `secret(SIZE)`, or by nothing (a
    /// stack's secret output, which state keeps no bytes of), `secret`;
    /// dim.
    pub fn secret(size_of: Option<usize>, json: Json) -> Cell {
        let text = match size_of {
            Some(n) => format!("secret({})", size(n)),
            None => "secret".into(),
        };
        Cell {
            text,
            paint: Some(Paint::Sensitive),
            json,
            string: None,
        }
    }

    /// The same cell with another `--json`.
    pub fn with_json(mut self, json: Json) -> Cell {
        self.json = json;
        self
    }

    /// The same cell painted `p`.
    pub fn painted(mut self, p: Paint) -> Cell {
        self.paint = Some(p);
        self
    }

    /// The cell's text, unfolded.
    pub fn text_of(&self) -> &str {
        &self.text
    }

    /// The cell's `--json`.
    pub fn json_of(&self) -> &Json {
        &self.json
    }

    /// What the cell prints as at `width`: a string longer than a screen
    /// is its first line and `.. (SIZE, N lines)`.
    fn shown(&self, width: usize) -> String {
        let Some(s) = &self.string else {
            return self.text.clone();
        };
        let lines = s.lines().count().max(1);
        if self.text.chars().count() <= width && lines <= SCREEN_LINES {
            return self.text.clone();
        }
        let first = format!("{:?}", s.lines().next().unwrap_or(""));
        let first = first.strip_suffix('"').unwrap_or(&first);
        let first: String = first.chars().take(FOLDED).collect();
        let n = if lines == 1 { "line" } else { "lines" };
        format!("{first} .. ({}, {lines} {n})", size(s.len()))
    }
}

/// Rows under named columns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    pub fn new<S: Into<String>>(columns: impl IntoIterator<Item = S>) -> Table {
        Table {
            columns: columns.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, row: Vec<Cell>) {
        debug_assert_eq!(row.len(), self.columns.len());
        self.rows.push(row);
    }

    /// The table without the columns no row has anything in (a listing's
    /// optional columns: a commit, a pending plan).
    pub fn without_empty_columns(mut self) -> Table {
        let keep: Vec<bool> = (0..self.columns.len())
            .map(|i| self.rows.iter().any(|r| !r[i].text.is_empty()))
            .collect();
        fn pick<T>(xs: Vec<T>, keep: &[bool]) -> Vec<T> {
            xs.into_iter()
                .zip(keep)
                .filter_map(|(x, k)| k.then_some(x))
                .collect()
        }
        self.columns = pick(std::mem::take(&mut self.columns), &keep);
        self.rows = std::mem::take(&mut self.rows)
            .into_iter()
            .map(|r| pick(r, &keep))
            .collect();
        self
    }

    /// The header line, the rows, and `(N rows)` past five or when there
    /// are none.
    pub fn render(&self, o: &Options) -> String {
        let mut out = self.lines(o, true);
        let n = self.rows.len();
        if n > 5 || n == 0 {
            out.push_str(&format!("({n} row{})\n", if n == 1 { "" } else { "s" }));
        }
        out
    }

    /// Two columns as a key/value table: the rows aligned, no header and
    /// no count (the scalars of `output`, say).
    pub fn pairs(&self, o: &Options) -> String {
        self.lines(o, false)
    }

    fn lines(&self, o: &Options, header: bool) -> String {
        let cells: Vec<Vec<(String, Option<Paint>)>> = self
            .rows
            .iter()
            .map(|row| row.iter().map(|c| (c.shown(o.width), c.paint)).collect())
            .collect();
        let mut width: Vec<usize> = if header {
            self.columns.iter().map(|c| c.chars().count()).collect()
        } else {
            vec![0; self.columns.len()]
        };
        for row in &cells {
            for (w, (c, _)) in width.iter_mut().zip(row) {
                *w = (*w).max(c.chars().count());
            }
        }
        let line = |cols: Vec<(&str, Option<Paint>)>| {
            let mut s = String::new();
            let last = cols.len().saturating_sub(1);
            for (i, ((c, p), w)) in cols.into_iter().zip(&width).enumerate() {
                s.push_str(&match p {
                    Some(p) => o.style.paint(p, c),
                    None => c.to_string(),
                });
                if i < last {
                    let pad = w - c.chars().count();
                    s.push_str(&" ".repeat(pad + 2));
                }
            }
            // An empty last column leaves no padding behind.
            let mut s = s.trim_end_matches(' ').to_string();
            s.push('\n');
            s
        };
        let mut out = String::new();
        if header {
            out.push_str(&line(
                self.columns
                    .iter()
                    .map(|c| (c.as_str(), Some(Paint::Bold)))
                    .collect(),
            ));
        }
        for row in &cells {
            out.push_str(&line(row.iter().map(|(c, p)| (c.as_str(), *p)).collect()));
        }
        out
    }

    /// An array of objects keyed by column.
    pub fn json(&self) -> Json {
        Json::Array(
            self.rows
                .iter()
                .map(|row| {
                    Json::Object(
                        self.columns
                            .iter()
                            .zip(row)
                            .map(|(k, c)| (k.clone(), c.json.clone()))
                            .collect(),
                    )
                })
                .collect(),
        )
    }
}

/// Tables one after another, each headed by its name (bold) and parted by
/// a blank line: each relation of `output`, say.
pub fn blocks(tables: &[(String, Table)], o: &Options) -> String {
    tables
        .iter()
        .map(|(name, t)| format!("{}\n{}", o.style.paint(Paint::Bold, name), t.render(o)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &[(&str, Value)]) -> Table {
        let mut t = Table::new(["name", "value"]);
        for (n, v) in rows {
            t.push(vec![Cell::text(*n), Cell::value(v, &Redactor::default())]);
        }
        t
    }

    #[test]
    fn columns_align_with_two_space_gutters_and_count_past_five() {
        let t = table(&[("a", Value::Int(1)), ("long", Value::Str("x".into()))]);
        assert_eq!(
            t.render(&Options::PLAIN),
            "name  value\na     1\nlong  \"x\"\n"
        );
        assert_eq!(t.pairs(&Options::PLAIN), "a     1\nlong  \"x\"\n");
        let six: Vec<(&str, Value)> = (0..6).map(|i| ("n", Value::Int(i))).collect();
        assert!(
            table(&six)
                .render(&Options::PLAIN)
                .ends_with("n     5\n(6 rows)\n")
        );
        assert_eq!(
            table(&[]).render(&Options::PLAIN),
            "name  value\n(0 rows)\n"
        );
    }

    #[test]
    fn a_string_wider_than_the_screen_folds_in_its_cell() {
        let t = table(&[("s", Value::Str("x".repeat(30)))]);
        let narrow = Options {
            width: 20,
            style: Style::PLAIN,
        };
        let folded = format!("\"{} .. (30 B, 1 line)", "x".repeat(30));
        assert_eq!(t.pairs(&narrow), format!("s  {folded}\n"));
        assert!(
            t.pairs(&Options::PLAIN)
                .contains(&format!("\"{}\"", "x".repeat(30)))
        );
    }

    #[test]
    fn json_is_an_array_of_objects_keyed_by_column() {
        let t = table(&[("a", Value::Int(1))]);
        assert_eq!(t.json(), serde_json::json!([{"name": "a", "value": 1}]));
    }

    #[test]
    fn colour_pads_by_the_text_not_the_escapes() {
        let o = Options {
            width: 100,
            style: Style { color: true },
        };
        let out = table(&[("a", Value::Int(1))]).render(&o);
        assert_eq!(out, "\x1b[1mname\x1b[0m  \x1b[1mvalue\x1b[0m\na     1\n");
    }
}
