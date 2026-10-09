//! A value as the report shows it (R-124): one side of a change redacted through
//! the one `Redactor` (`Shown`), an expression masked where its value is a
//! secret's, a long string elided, a host a reader may mistake for another
//! named.

use super::Why;
use super::labels::{printed_attribute, printed_label, reference};
use super::lines::reads;
use crate::ir::Address;
use crate::provider::{NULL_KEY, json_to_value, marker};
use crate::query::Redactor;
use crate::schema::Schema;
use crate::spell;
use crate::value::Value;
use serde_json::{Value as Json, json};

/// Past this many characters a string elides its middle at the default
/// level (R-111): a public key, a digest.
pub(super) const LONG: usize = 60;

/// `s` with its middle elided when it is longer than [`LONG`].
pub(crate) fn elide(s: &str) -> String {
    let n = s.chars().count();
    if n <= LONG {
        return s.to_string();
    }
    let (head, tail) = (LONG / 2, LONG - 1 - LONG / 2);
    let head: String = s.chars().take(head).collect();
    let tail: String = s.chars().skip(n - tail).collect();
    format!("{head}…{tail}")
}

/// One side of a change, after redaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Shown {
    Absent,
    Value(Json),
    /// A null, by its printed label (`T["A"].p`).
    Null {
        label: String,
        class: String,
    },
    /// A secret (with its printed label) or a value at a sensitive path
    /// (none).
    Sensitive(Option<String>),
    /// A reference whose resource exists: the resource's address, and the
    /// id the provider resolved it to (`--json` shows the id; R-43).
    Ref {
        addr: Address,
        value: Json,
    },
}

/// Redact one side of a provider change: a null or secret marker is its
/// label; a value at a sensitive path is `(sensitive)`; a value that is, or
/// holds, a secret the program set elsewhere is that secret's label.
pub fn shown(v: Option<&Json>, sensitive: bool, schema: &Schema, r: &Redactor) -> Shown {
    let Some(v) = v else {
        return Shown::Absent;
    };
    match marker(v) {
        Some((NULL_KEY, l)) => Shown::Null {
            label: crate::ir::label(l),
            class: null_class(l, schema),
        },
        Some((_, l)) => Shown::Sensitive(Some(crate::ir::label(l))),
        // At a sensitive path, a derived secret by the call that derived
        // it (`random.password("db")`), anything else bare.
        None if sensitive || has_secret(v) => {
            Shown::Sensitive(r.derived(&json_to_value(v)).map(str::to_string))
        }
        None => match secret_in(&r.json(&json_to_value(v))) {
            Some(l) => Shown::Sensitive(Some(l)),
            None => Shown::Value(v.clone()),
        },
    }
}

/// `v`, the part of `whole` at `keys`, as `why` and a chain print it: a
/// plain leaf of a secret object (`data.user = "synapse"` in a Secret's
/// data, a literal written into a part a secret is written to) is
/// `(sensitive)`, though its value is no secret elsewhere; a secret by its
/// label; a value not known yet, and any other, as the program writes it
/// (R-124 amendment 2, R-128).
pub fn surface_in(r: &Redactor, whole: &Value, keys: &[String], v: &Value) -> String {
    let mut at = Some(whole);
    let mut hidden = false;
    for k in keys {
        let Some(x) = at else { break };
        hidden |= r.is_secret(x);
        at = match x {
            Value::Obj(m) => m.get(k),
            _ => None,
        };
    }
    // A value not known yet is the reference it is, as the plan says it.
    match hidden && !r.is_secret(v) && !matches!(v, Value::Null { .. }) {
        true => "(sensitive)".into(),
        false => r.surface(v),
    }
}

/// Expression `e` as written where its value is a secret's (R-124
/// amendment 2): a literal is `(sensitive)`; in one that reads something,
/// each string literal outside a call's arguments (`{ user: "synapse",
/// password: random.password("db") }` says `{ user: (sensitive),
/// password: random.password("db") }`).
pub fn masked(e: &str) -> String {
    if !reads(e) {
        return "(sensitive)".into();
    }
    let mut out = String::new();
    let mut open: Vec<char> = Vec::new();
    let mut cs = e.chars().peekable();
    while let Some(c) = cs.next() {
        match c {
            '(' | '[' | '{' => open.push(c),
            ')' | ']' | '}' => {
                open.pop();
            }
            '"' => {
                let mut lit = String::from('"');
                let mut esc = false;
                for d in cs.by_ref() {
                    lit.push(d);
                    match d {
                        '\\' if !esc => esc = true,
                        '"' if !esc => break,
                        _ => esc = false,
                    }
                }
                match open.last() == Some(&'(') {
                    true => out.push_str(&lit),
                    false => out.push_str("(sensitive)"),
                }
                continue;
            }
            _ => {}
        }
        out.push(c);
    }
    out
}

/// Redact a fact store value: [`Redactor::json`], one side of a line.
pub fn shown_value(v: &Value, r: &Redactor) -> Shown {
    match r.json(v) {
        Json::Object(m) if m.len() == 2 && m.contains_key("null") => Shown::Null {
            label: m["null"].as_str().unwrap_or_default().to_string(),
            class: m["class"].as_str().unwrap_or_default().to_string(),
        },
        j => match secret_in(&j) {
            Some(l) => Shown::Sensitive(Some(l)),
            None => Shown::Value(j),
        },
    }
}

/// The label of the first `{"sensitive": label}` inside a redacted value.
fn secret_in(v: &Json) -> Option<String> {
    match v {
        Json::Object(m) if m.len() == 1 && m.contains_key("sensitive") => {
            Some(m["sensitive"].as_str().unwrap_or_default().to_string())
        }
        Json::Object(m) => m.values().find_map(secret_in),
        Json::Array(xs) => xs.iter().find_map(secret_in),
        _ => None,
    }
}

/// A secret marker anywhere inside a value.
fn has_secret(v: &Json) -> bool {
    match (marker(v), v) {
        (Some((NULL_KEY, _)), _) => false,
        (Some(_), _) => true,
        (None, Json::Array(xs)) => xs.iter().any(has_secret),
        (None, Json::Object(m)) => m.values().any(has_secret),
        _ => false,
    }
}

/// A null's class from the schema, by its label `type/addr#path`.
pub(super) fn null_class(label: &str, schema: &Schema) -> String {
    let Some((t, _, p)) = crate::value::null_parts(label) else {
        return "unknown".into();
    };
    schema
        .class_of(&t, &p)
        .or_else(|| schema.optional_computed_class(&t, &p))
        .map(|c| c.name().to_string())
        .unwrap_or_else(|| "unknown".into())
}

impl Shown {
    pub fn text(&self) -> String {
        match self {
            Shown::Absent => "<none>".into(),
            Shown::Value(Json::String(s)) => {
                format!("{}{}", spell::quote(s), host_ascii_text(s))
            }
            Shown::Value(v) => serde_json::to_string(v).unwrap_or_else(|_| "<unprintable>".into()),
            Shown::Null { label, .. } => format!("?{label}"),
            Shown::Sensitive(Some(l)) => format!("(sensitive {l})"),
            Shown::Sensitive(None) => "(sensitive)".into(),
            Shown::Ref { addr, .. } => addr.to_string(),
        }
    }

    /// The value as the plan prints it at level `why` (R-111): a reference
    /// or a value not known yet as the reference it is (`k3s.server`,
    /// `k3s.server.public_ip`), with no `?`; a secret `(sensitive)`, by its
    /// label from `-v`; a long string elided in the middle at the default
    /// level. `-q` prints [`Shown::text`].
    pub fn said(&self, why: Why) -> String {
        match self {
            Shown::Null { label: l, .. } => printed_label(l),
            Shown::Sensitive(Some(l)) if why >= Why::How => {
                format!("(sensitive {})", printed_attribute(l))
            }
            // A rotated secret says why it changes at every level (R-161).
            Shown::Sensitive(Some(l)) if why > Why::None => {
                match l.split_once(&format!(", {}", crate::functions::random::ROTATED)) {
                    Some((_, r)) => {
                        format!("(sensitive, {}{r})", crate::functions::random::ROTATED)
                    }
                    None => "(sensitive)".into(),
                }
            }
            Shown::Sensitive(_) => "(sensitive)".into(),
            Shown::Ref { addr, .. } => reference(addr, ""),
            Shown::Value(Json::String(s)) if why == Why::Line => {
                format!("{}{}", spell::quote(&elide(s)), host_ascii_text(s))
            }
            _ => self.text(),
        }
    }

    pub fn json(&self) -> Json {
        match self {
            Shown::Absent => Json::Null,
            Shown::Value(v) => v.clone(),
            Shown::Null { label, class } => json!({"null": label, "class": class}),
            Shown::Sensitive(l) => json!({ "sensitive": l }),
            Shown::Ref { value, .. } => value.clone(),
        }
    }
}

/// A host with a label that is not ASCII prints both forms (R-134): the
/// text as written, then two spaces and its A-labels, what a provider
/// receives (`"bücher.example"  xn--bcher-kva.example`); at every level,
/// and carried by no colour. Empty for any other text.
fn host_ascii_text(s: &str) -> String {
    crate::uri::ascii_form(s)
        .map(|a| format!("  {a}"))
        .unwrap_or_default()
}

/// The host labels in a value a reader may mistake for others (R-134,
/// UTS 39): `host label "pаypal" mixes Latin and Cyrillic  ATTRIBUTE  SITE`,
/// for every string inside it.
pub(super) fn confusables_in(v: &Json, attr: &str, at: &str, out: &mut Vec<String>) {
    match v {
        Json::String(s) => {
            for (label, why) in crate::uri::confusable_labels(s) {
                let line = format!("host label {label:?} {why}  {attr}{at}");
                if !out.contains(&line) {
                    out.push(line);
                }
            }
        }
        Json::Array(xs) => xs.iter().for_each(|x| confusables_in(x, attr, at, out)),
        Json::Object(m) => m.values().for_each(|x| confusables_in(x, attr, at, out)),
        _ => {}
    }
}

/// A site column's text with its expression [`masked`]: the part before
/// three spaces (`EXPR   FILE:LINE`), or all of it when it is no place.
pub(super) fn masked_text(t: &str) -> String {
    let place = |x: &str| {
        x.trim()
            .rsplit_once(':')
            .is_some_and(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
    };
    match t.split_once("   ") {
        Some((e, rest)) => format!("{}   {rest}", masked(e)),
        None if place(t) || t.trim().is_empty() || t.starts_with('@') => t.to_string(),
        None => masked(t),
    }
}
