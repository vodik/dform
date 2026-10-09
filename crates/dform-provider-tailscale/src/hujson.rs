//! HuJSON, the policy file's format: JSON with `//` and `/* */` comments
//! and trailing commas. The API takes and answers it; the provider
//! compares a policy by its canonical form, the standard JSON with every
//! object's keys sorted and no space, which is what `json.encode` writes,
//! so a comment or a reordering in the console is no change and an edit
//! of a rule is.

use serde_json::{Map, Value as Json};

/// `text` as standard JSON: comments blanked and trailing commas dropped,
/// what is inside a string left as it is.
pub fn standardize(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                out.push_str(&text[start..i]);
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                out.push(' ');
            }
            b',' => {
                // A comma before a closing bracket, past space and
                // comments, is a trailing one.
                if !matches!(next_significant(b, i + 1), Some(b'}' | b']')) {
                    out.push(',');
                }
                i += 1;
            }
            _ => {
                let c = text[i..].chars().next().expect("in bounds");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// The first byte from `i` that is not space or inside a comment.
fn next_significant(b: &[u8], mut i: usize) -> Option<u8> {
    while i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            c => return Some(c),
        }
    }
    None
}

/// `text` (HuJSON or JSON) parsed.
pub fn parse(text: &str) -> Result<Json, String> {
    serde_json::from_str(&standardize(text)).map_err(|e| e.to_string())
}

/// `v` with every object's keys sorted, at every depth.
fn sorted(v: Json) -> Json {
    match v {
        Json::Object(m) => {
            let mut kv: Vec<(String, Json)> = m.into_iter().collect();
            kv.sort_by(|a, b| a.0.cmp(&b.0));
            Json::Object(
                kv.into_iter()
                    .map(|(k, v)| (k, sorted(v)))
                    .collect::<Map<_, _>>(),
            )
        }
        Json::Array(xs) => Json::Array(xs.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// The canonical text of a policy: standard JSON, keys sorted, no space.
pub fn canonical(text: &str) -> Result<String, String> {
    Ok(sorted(parse(text)?).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_trailing_commas_are_not_the_policy() {
        let hujson = r#"{
  // who may use which tag
  "tagOwners": {"tag:k8s": ["autogroup:admin"],},
  /* the rules */
  "acls": [
    {"action": "accept", "src": ["tag:admin"], "dst": ["tag:k8s:22,6443"]}, // ssh and the api
  ],
}"#;
        assert_eq!(
            canonical(hujson).unwrap(),
            r#"{"acls":[{"action":"accept","dst":["tag:k8s:22,6443"],"src":["tag:admin"]}],"tagOwners":{"tag:k8s":["autogroup:admin"]}}"#
        );
    }

    #[test]
    fn a_string_keeps_what_looks_like_a_comment() {
        let t = r#"{"a": "http://x/*y*/", "b": "q\"//,]"}"#;
        assert_eq!(
            canonical(t).unwrap(),
            r#"{"a":"http://x/*y*/","b":"q\"//,]"}"#
        );
    }

    #[test]
    fn json_encode_writes_the_canonical_form() {
        let t = r#"{"acls":[{"action":"accept","dst":["*:*"],"src":["*"]}]}"#;
        assert_eq!(canonical(t).unwrap(), t);
        assert!(canonical("{").is_err());
    }
}
