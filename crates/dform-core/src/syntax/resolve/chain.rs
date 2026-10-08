//! A chain as written, `a.b[e].c` or `f(x).p`: its head and the parts
//! after it, parsed off the tree before anything resolves it.

use super::*;

/// A part of a chain after its head.
#[derive(Clone)]
pub(super) enum Op {
    Field(String),
    Index(Vec<SyntaxNode>, rowan::TextRange),
    /// `[k=v, ..]`: a stack's deployment by its keys (R-65).
    Keyed(Vec<(String, SyntaxNode)>, rowan::TextRange),
}

/// A chain: its head word and the parts after it.
#[derive(Clone)]
pub(super) struct Chain {
    pub(super) head: String,
    pub(super) head_kind: SyntaxKind,
    /// The call a `CALL_CHAIN` reads the result of (R-71), `f(x).p`; its
    /// `head` is then empty.
    pub(super) call: Option<SyntaxNode>,
    pub(super) range: rowan::TextRange,
    pub(super) ops: Vec<Op>,
}

impl Chain {
    /// A chain that starts with a name.
    pub(super) fn of(n: &SyntaxNode) -> Option<Chain> {
        Chain::read(n).filter(|c| c.call.is_none())
    }

    /// A chain, or a read of a call's result (R-71): what `has`, a truth
    /// test and a term resolve.
    pub(super) fn read(n: &SyntaxNode) -> Option<Chain> {
        if !matches!(n.kind(), CHAIN | CALL_CHAIN) {
            return None;
        }
        let mut it = n
            .children_with_tokens()
            .filter(|e| e.as_token().is_none_or(|t| !t.kind().is_trivia()));
        let (head, head_kind, call) = match it.next()? {
            rowan::NodeOrToken::Node(c) if c.kind() == CALL => (String::new(), CALL, Some(c)),
            rowan::NodeOrToken::Node(_) => return None,
            rowan::NodeOrToken::Token(t) => (t.text().to_string(), t.kind(), None),
        };
        let mut ops = Vec::new();
        let mut pending: Option<SyntaxKind> = None;
        for e in it {
            match e {
                rowan::NodeOrToken::Node(ix) if ix.kind() == INDEX => {
                    let mut named: Vec<(String, SyntaxNode)> = ix
                        .children()
                        .filter(|c| c.kind() == NAMED_ARG)
                        .filter_map(|a| Some((word_text(&a, 0), terms(&a).next()?)))
                        .collect();
                    // Beside a `k = v`, a bare name is the pun `k = k`
                    // (R-33): `platform[env, region = "GRA11"]`.
                    if !named.is_empty() {
                        let puns: Vec<(String, SyntaxNode)> = terms(&ix)
                            .filter_map(|t| Some((bare_name(&t)?, t)))
                            .collect();
                        named.extend(puns);
                    }
                    ops.push(match named.is_empty() {
                        true => Op::Index(terms(&ix).collect(), ix.text_range()),
                        false => Op::Keyed(named, ix.text_range()),
                    });
                }
                rowan::NodeOrToken::Node(_) => {}
                rowan::NodeOrToken::Token(t) => match (pending, t.kind()) {
                    (None, DOT) => pending = Some(t.kind()),
                    (Some(DOT), STRING) => {
                        let s = crate::syntax::resolve::unescape(t.text()).unwrap_or_default();
                        ops.push(Op::Field(s));
                        pending = None;
                    }
                    (Some(DOT), _) => {
                        ops.push(Op::Field(t.text().to_string()));
                        pending = None;
                    }
                    _ => {}
                },
            }
        }
        Some(Chain {
            head,
            head_kind,
            call,
            range: n.text_range(),
            ops,
        })
    }

    /// The leading words: the head and the `.name` parts right after it.
    pub(super) fn fields(&self) -> Vec<String> {
        let mut out = vec![self.head.clone()];
        for op in &self.ops {
            match op {
                Op::Field(f) => out.push(f.clone()),
                _ => break,
            }
        }
        out
    }

    pub(super) fn is_bare(&self) -> bool {
        self.ops.is_empty()
    }
}
