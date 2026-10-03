//! Doc comments (docs/grammar.md "Doc comments"): `#|` lines directly
//! above a component, input, output, predicate, rule, type alias or
//! resource. `#| key: value` is a pair, any other `#|` line is
//! part of the item's `description`. They lower to `doc(Kind, Name, Key,
//! Value)` facts; the language server shows them and `dform doc` renders
//! them.

use super::SyntaxKind::{self, *};
use super::{SyntaxNode, SyntaxToken};
use rowan::TextRange;

/// The keys docs/grammar.md documents; any other key is kept as written.
pub const KEYS: &[&str] = &["description", "owner", "since", "deprecated"];

/// One documented item.
#[derive(Debug, Clone)]
pub struct Doc {
    /// `component`, `input`, `output`, `predicate`, `rule`, `alias` or
    /// `resource`.
    pub kind: &'static str,
    /// Its name; inside a component `COMPONENT.NAME`.
    pub name: String,
    /// The pairs in order, the description (bare lines joined) first.
    pub pairs: Vec<(String, String)>,
    /// The documented statement.
    pub node: SyntaxNode,
    /// The `#|` lines.
    pub range: TextRange,
}

impl Doc {
    /// The first value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// The `#|` lines directly above `n`, parsed, and where they are.
pub fn comment(n: &SyntaxNode) -> Option<(TextRange, Vec<(String, String)>)> {
    let mut lines: Vec<SyntaxToken> = Vec::new();
    let mut t = n.first_token()?.prev_token();
    loop {
        // Exactly one line break between a doc line and what follows it.
        let Some(ws) = t.filter(|w| w.kind() == WHITESPACE && w.text().matches('\n').count() == 1)
        else {
            break;
        };
        let Some(c) = ws
            .prev_token()
            .filter(|c| c.kind() == COMMENT && c.text().starts_with("#|"))
        else {
            break;
        };
        // A `#|` after code on its line is that line's comment.
        let before = c.prev_token();
        if before
            .as_ref()
            .is_some_and(|b| !(b.kind() == WHITESPACE && b.text().contains('\n')))
        {
            break;
        }
        lines.push(c);
        t = before;
    }
    let (first, last) = (lines.last()?, lines.first()?);
    let range = TextRange::new(first.text_range().start(), last.text_range().end());
    lines.reverse();
    Some((range, pairs(lines.iter().map(|t| t.text()))))
}

/// `#|` lines' pairs: `key: value` where the line starts with a lowercase
/// name and a colon, else a line of the description.
pub fn pairs<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut description: Vec<&str> = Vec::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for l in lines {
        let l = l.strip_prefix("#|").unwrap_or(l);
        let l = l.strip_prefix(' ').unwrap_or(l).trim_end();
        match key_value(l) {
            Some(("description", v)) => description.push(v),
            Some((k, v)) => out.push((k.to_string(), v.to_string())),
            None => description.push(l),
        }
    }
    let text = description.join("\n").trim().to_string();
    if !text.is_empty() {
        out.insert(0, ("description".to_string(), text));
    }
    out
}

fn key_value(l: &str) -> Option<(&str, &str)> {
    let (k, v) = l.split_once(':')?;
    let mut cs = k.chars();
    let ok = cs.next().is_some_and(|c| c.is_ascii_lowercase())
        && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    ok.then(|| (k, v.trim()))
}

fn tokens(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
}

fn is_word(k: SyntaxKind) -> bool {
    k == IDENT || k.is_keyword()
}

/// The `skip`th word of a node's own tokens.
fn word(n: &SyntaxNode, skip: usize) -> Option<String> {
    tokens(n)
        .filter(|t| is_word(t.kind()))
        .nth(skip)
        .map(|t| t.text().to_string())
}

/// `a.b.c` from the node's own tokens after its first word.
fn dotted(n: &SyntaxNode) -> Option<String> {
    let mut out = String::new();
    for t in tokens(n).skip(1) {
        match t.kind() {
            k if is_word(k) && !out.ends_with(|c: char| c.is_alphanumeric() || c == '_') => {
                out.push_str(t.text())
            }
            DOT if !out.is_empty() => out.push('.'),
            _ => break,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A string token's text without its quotes (escapes as written).
fn unquote(t: &SyntaxToken) -> String {
    let s = t.text();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

/// The kind and name (unqualified) of a statement a doc comment may
/// document; `None` for anything else.
pub fn item(n: &SyntaxNode) -> Option<(&'static str, String)> {
    Some(match n.kind() {
        COMPONENT => ("component", word(n, 1)?),
        INPUT => ("input", word(n, 1)?),
        INPUT_RELATION => ("predicate", word(n, 1)?),
        OUTPUT_DECL => ("output", word(n, 1)?),
        TYPE_ALIAS => ("alias", word(n, 1)?),
        DECL => ("predicate", dotted(n)?),
        EXTERN => ("predicate", dotted(n)?),
        RULE | FACT => {
            let call = n.children().find(|c| c.kind() == CALL)?;
            let chain = call.children().find(|c| c.kind() == CHAIN)?;
            ("rule", chain.text().to_string())
        }
        LET => ("rule", word(n, 1)?),
        CHECK => ("rule", unquote(&tokens(n).find(|t| t.kind() == STRING)?)),
        RESOURCE => {
            // `resource net.vpc vpc @rank`: the type's words and dots, then
            // the name (a word or a string); named as its address,
            // `net.vpc["vpc"]`.
            let ts: Vec<SyntaxToken> = tokens(n).filter(|t| t.kind() != RANK).skip(1).collect();
            let (name, typ) = ts.split_last()?;
            let name = match name.kind() {
                STRING => unquote(name),
                k if is_word(k) => name.text().to_string(),
                _ => return None,
            };
            let typ: String = typ.iter().map(|t| t.text()).collect();
            ("resource", format!("{typ}[\"{name}\"]"))
        }
        _ => return None,
    })
}

/// The component a statement is in.
pub fn enclosing(n: &SyntaxNode) -> Option<String> {
    n.ancestors()
        .skip(1)
        .find(|a| a.kind() == COMPONENT)
        .and_then(|a| word(&a, 1))
}

/// Every documented item of a file's tree, in source order.
pub fn collect(root: &SyntaxNode) -> Vec<Doc> {
    let mut out = Vec::new();
    for n in root.descendants() {
        let Some((kind, name)) = item(&n) else {
            continue;
        };
        let Some((range, pairs)) = comment(&n) else {
            continue;
        };
        if pairs.is_empty() {
            continue;
        }
        let name = match enclosing(&n) {
            Some(b) => format!("{b}.{name}"),
            None => name,
        };
        out.push(Doc {
            kind,
            name,
            pairs,
            node: n,
            range,
        });
    }
    out
}

/// The statement's first line, without the block it opens: what a
/// rendering shows above its docs.
pub fn header(n: &SyntaxNode) -> String {
    let text = n.text().to_string();
    let line = text.lines().next().unwrap_or("").trim_end();
    let line = line.strip_suffix('{').unwrap_or(line).trim_end();
    line.strip_suffix(" if").unwrap_or(line).to_string()
}

/// `dform doc`: every documented item of `files` (each a display name and
/// its tree), as Markdown under the heading `title`: per file, per item
/// its kind and name, its statement's first line, its description and its
/// other pairs.
pub fn markdown(title: &str, files: &[(String, SyntaxNode)]) -> String {
    let mut out = format!("# {title}\n");
    for (name, root) in files {
        let docs = collect(root);
        if docs.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {name}\n"));
        for d in docs {
            out.push_str(&format!(
                "\n### {} `{}`\n\n```dform\n{}\n```\n",
                d.kind,
                d.name,
                header(&d.node)
            ));
            if let Some(text) = d.get("description") {
                out.push_str(&format!("\n{text}\n"));
            }
            let rest: Vec<&(String, String)> =
                d.pairs.iter().filter(|(k, _)| k != "description").collect();
            if !rest.is_empty() {
                out.push('\n');
                for (k, v) in rest {
                    out.push_str(&format!("- **{k}**: {v}\n"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type Pairs = Vec<(String, String)>;

    fn docs(src: &str) -> Vec<(String, String, Pairs)> {
        let root = super::super::parser::parse(src).syntax();
        collect(&root)
            .into_iter()
            .map(|d| (d.kind.to_string(), d.name, d.pairs))
            .collect()
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn doc_lines_directly_above_an_item_document_it() {
        let src = "\n\n#| The network.\n#| owner: platform\n#|\n#| Its subnets are private.\ncomponent network {\n  #| The VPC's range.\n  #| since: 2026.1\n  input vpc_net: inet\n\n  #| not this: a blank line follows\n\n  output vpc: net.vpc\n  #| A subnet.\n  resource net.subnet \"private-${z}\" @default {\n  } where data(\"zone\", z)\n}\nlet x = 1 #| a trailing comment\nlet y = 2\n# a plain comment\n#| deprecated: use q\np(a) where q(a)\n#| Checked.\ndeny \"no\" where p(1)\n#| Named.\ntype env = enum(\"a\")\n#| An extern.\nextern dns.lookup(+name, -addr)\n";
        assert_eq!(
            docs(src),
            vec![
                (
                    "component".into(),
                    "network".into(),
                    kv(&[
                        ("description", "The network.\n\nIts subnets are private."),
                        ("owner", "platform")
                    ])
                ),
                (
                    "input".into(),
                    "network.vpc_net".into(),
                    kv(&[("description", "The VPC's range."), ("since", "2026.1")])
                ),
                (
                    "resource".into(),
                    "network.net.subnet[\"private-${z}\"]".into(),
                    kv(&[("description", "A subnet.")])
                ),
                ("rule".into(), "p".into(), kv(&[("deprecated", "use q")])),
                (
                    "rule".into(),
                    "no".into(),
                    kv(&[("description", "Checked.")])
                ),
                (
                    "alias".into(),
                    "env".into(),
                    kv(&[("description", "Named.")])
                ),
                (
                    "predicate".into(),
                    "dns.lookup".into(),
                    kv(&[("description", "An extern.")])
                ),
            ]
        );
    }

    #[test]
    fn a_capitalised_word_and_a_colon_is_prose() {
        assert_eq!(
            pairs(["#| Note: this", "#|owner:me", "#| description: more"].into_iter()),
            kv(&[("description", "Note: this\nmore"), ("owner", "me")])
        );
    }
}
