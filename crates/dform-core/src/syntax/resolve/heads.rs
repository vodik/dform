//! The first segment of a dotted name (DESIGN.org R-6): one registry
//! decides what `head` in `head.rest` is, and a head two things claim is an
//! error naming both.
//!
//! | Head                 | Claimed by                                     |
//! |----------------------|------------------------------------------------|
//! | type namespace       | a dotted type (`net` of `net.vpc`)             |
//! | provider             | its externs (`file` of `file.json`)            |
//! | function package     | `std/*.df` (`inet` of `inet.subnet`)           |
//! | module               | `module network { .. }` (`network.main.vpc`)   |
//! | root                 | `settings`, `world`, `stacks`                  |
//!
//! A provider's types and its externs share its name (`dns.record`,
//! `dns.lookup`): one owner, not a collision. A module named like a type
//! namespace (examples/demo's `module iam` beside `iam.role`) is left to
//! the resolver's order for now: the check is of the heads the compiler
//! itself adds (function packages, roots) against the program's.

use super::*;

/// What a dotted name's first segment can be.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Head {
    /// A type namespace, or a provider's externs: the provider's name.
    Provider,
    Package,
    Module,
    Root,
}

const ROOTS: &[&str] = &["settings", "world", "stacks"];

impl Lowerer<'_> {
    /// Every head the program and `std/*.df` claim, checked for collisions.
    pub(super) fn check_heads(&mut self) {
        // head -> (what, a span in the program that claims it, its words).
        let mut claims: BTreeMap<String, Vec<(Head, Option<Span>, String)>> = BTreeMap::new();
        let mut claim = |head: &str, what: Head, span: Option<Span>, words: String| {
            let c = claims.entry(head.to_string()).or_default();
            if !c.iter().any(|(w, _, _)| *w == what) {
                c.push((what, span, words));
            }
        };
        for r in ROOTS {
            claim(r, Head::Root, None, format!("the root `{r}`"));
        }
        let fns = crate::functions::registry();
        for p in fns.packages() {
            let file = fns
                .functions()
                .find(|f| f.package == p)
                .map(|f| f.file.clone())
                .unwrap_or_default();
            claim(p, Head::Package, None, format!("the function package `{p}` ({file})"));
        }
        for u in self.units {
            let span = |n: &SyntaxNode| {
                let r = n.text_range();
                Span {
                    file: u.file,
                    start: u32::from(r.start()),
                    end: u32::from(r.end()),
                    origin: 0,
                }
            };
            for n in u.root.descendants() {
                match n.kind() {
                    MODULE => {
                        let m = word_text(&n, 1);
                        claim(&m, Head::Module, Some(span(&n)), format!("the module `{m}`"));
                    }
                    EXTERN | RESOURCE | TYPE_DECL => {
                        let name = dotted_text(&n, 1);
                        if let Some((h, _)) = name.split_once('.') {
                            let what = if n.kind() == EXTERN {
                                format!("the extern `{name}`")
                            } else {
                                format!("the type `{name}`")
                            };
                            claim(h, Head::Provider, Some(span(&n)), what);
                        }
                    }
                    _ => {}
                }
            }
        }
        let dotted: Vec<String> = self
            .decls
            .types
            .iter()
            .filter_map(|t| t.split_once('.').map(|(h, _)| h.to_string()))
            .collect();
        for ns in dotted {
            claim(&ns, Head::Provider, None, format!("the type namespace `{ns}`"));
        }
        for (head, mut cs) in claims {
            let compilers = cs
                .iter()
                .filter(|(w, _, _)| matches!(w, Head::Package | Head::Root))
                .count();
            if cs.len() < 2 || compilers == 0 {
                continue;
            }
            cs.sort_by_key(|(_, s, _)| s.is_none());
            let (_, at, a) = &cs[0];
            let (_, _, b) = &cs[1];
            self.diags.push(
                Diagnostic::error(
                    at.unwrap_or_default(),
                    format!("`{head}` is both {a} and {b}"),
                )
                .with_help(format!(
                    "the first segment of a dotted name names one thing: rename the {}",
                    if cs[0].0 == Head::Module { "module" } else { "declaration" }
                )),
            );
        }
    }
}
