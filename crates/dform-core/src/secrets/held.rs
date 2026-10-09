//! A secret a provider holds, inside a string template (R-218).
//!
//! dform never has the bytes of a secret a provider holds (a resource's
//! sensitive computed value, an extern's secret column, another stack's
//! held output): the program has its label, a secret null, which no
//! evaluation ever fills. Written whole it travels as its label
//! (`{"$secret": label}`). Written inside a string template (`"authkey
//! ${nodes.key}"`), the template is a string holding a placeholder where
//! the secret goes ([`placeholder`]), so the template is a value at once
//! and waits for nothing. The engine reveals each into the call that
//! writes it (a provider's Apply, its Configure), under the deployment's
//! lease, and nowhere else: state, the plan file, the audit log and every
//! message hold the placeholder, a label, digested where a value is.
//!
//! Only a template composes a secret dform does not have: its bytes are
//! put in place as text. Any other function over one (an encoder, a hash)
//! would need them, so it has no value; `deployment` says so as an error.
//!
//! A placeholder is the label between two control characters, and only one
//! this process minted is one: text read from elsewhere that looks like
//! one is text.

use crate::value::{NullClass, Value};
use anyhow::Result;
use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

const OPEN: char = '\u{1}';
const CLOSE: char = '\u{2}';

/// The labels this process minted a placeholder for.
static MINTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn minted() -> MutexGuard<'static, BTreeSet<String>> {
    MINTED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The placeholder of the held secret `label` inside a template.
pub fn placeholder(label: &str) -> String {
    minted().insert(label.to_string());
    format!("{OPEN}{label}{CLOSE}")
}

/// The arguments of the function `name` with each held secret among them
/// as its placeholder, when `name` is a string template whose arguments'
/// nulls are all secrets. `None` otherwise: a template over a value not
/// known yet waits for it, and no other function reads a secret dform
/// does not have.
pub fn in_template(name: &str, vals: &[Value]) -> Option<Vec<Value>> {
    if name != crate::ir::FORMAT {
        return None;
    }
    vals.iter()
        .map(|v| match v {
            Value::Null {
                label,
                class: NullClass::Secret,
                ..
            } => Some(Value::Str(placeholder(label))),
            v if crate::stuck::has_null(v) => None,
            v => Some(v.clone()),
        })
        .collect()
}

/// A template's text and the held secrets between it.
enum Part<'a> {
    Text(&'a str),
    Held(&'a str),
}

fn parts(text: &str) -> Vec<Part<'_>> {
    if !text.contains(OPEN) {
        return vec![Part::Text(text)];
    }
    let minted = minted();
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len_utf8()..];
        match after.find(CLOSE) {
            Some(close) if minted.contains(&after[..close]) => {
                out.push(Part::Text(&rest[..open]));
                out.push(Part::Held(&after[..close]));
                rest = &after[close + CLOSE.len_utf8()..];
            }
            _ => {
                out.push(Part::Text(&rest[..open + OPEN.len_utf8()]));
                rest = after;
            }
        }
    }
    out.push(Part::Text(rest));
    out
}

/// The labels of the held secrets `text` holds, in order.
pub fn labels(text: &str) -> Vec<String> {
    parts(text)
        .into_iter()
        .filter_map(|p| match p {
            Part::Held(l) => Some(l.to_string()),
            Part::Text(_) => None,
        })
        .collect()
}

/// Whether `text` holds a held secret.
pub fn carries(text: &str) -> bool {
    parts(text).iter().any(|p| matches!(p, Part::Held(_)))
}

/// `text` with each held secret in it revealed by `reveal`, which answers
/// a label's bytes as text.
pub fn fill(text: &str, mut reveal: impl FnMut(&str) -> Result<String>) -> Result<String> {
    let mut out = String::new();
    for p in parts(text) {
        match p {
            Part::Text(t) => out.push_str(t),
            Part::Held(l) => out.push_str(&reveal(l)?),
        }
    }
    Ok(out)
}

/// `text` as the program wrote it: each held secret `${..}`, by the name
/// the program reads it by.
pub fn shown(text: &str) -> String {
    parts(text)
        .into_iter()
        .map(|p| match p {
            Part::Text(t) => t.to_string(),
            Part::Held(l) => format!("${{{}}}", crate::ir::label(l)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Secret,
            ty: "string".into(),
        }
    }

    #[test]
    fn a_template_holds_its_secrets_in_place_and_is_filled_with_their_bytes() {
        let vals = [
            Value::Str("authkey %s; again %s".into()),
            secret("tailscale.auth_key/nodes#key"),
            secret("tailscale.auth_key/nodes#key"),
        ];
        let args = in_template(crate::ir::FORMAT, &vals).unwrap();
        let Some(Value::Str(text)) = crate::functions::body(crate::ir::FORMAT).unwrap()(&args)
        else {
            panic!("a template is a string");
        };
        assert_eq!(labels(&text), ["tailscale.auth_key/nodes#key"; 2]);
        assert_eq!(
            shown(&text),
            "authkey ${tailscale.auth_key[\"nodes\"].key}; again \
             ${tailscale.auth_key[\"nodes\"].key}"
        );
        let filled = fill(&text, |l| Ok(format!("<{l}>"))).unwrap();
        assert_eq!(
            filled,
            "authkey <tailscale.auth_key/nodes#key>; again <tailscale.auth_key/nodes#key>"
        );
    }

    /// A template over a value not known yet waits for it; no other
    /// function reads a held secret.
    #[test]
    fn only_a_template_over_secrets_alone_has_a_value() {
        let open = Value::Null {
            label: "net.vpc/main#id".into(),
            class: NullClass::Open,
            ty: "string".into(),
        };
        let fmt = Value::Str("%s %s".into());
        let s = secret("vault.token/t#value");
        assert!(in_template(crate::ir::FORMAT, &[fmt.clone(), s.clone(), open]).is_none());
        assert!(in_template("json.encode", &[s.clone()]).is_none());
        assert!(in_template(crate::ir::FORMAT, &[fmt, s, Value::Int(1)]).is_some());
    }

    /// Text that looks like a placeholder no evaluation minted is text.
    #[test]
    fn a_placeholder_read_from_elsewhere_is_text() {
        let forged = format!("{OPEN}vault.token/forged#value{CLOSE} and {OPEN}");
        assert!(!carries(&forged));
        assert_eq!(fill(&forged, |_| panic!("no reveal")).unwrap(), forged);
    }
}
