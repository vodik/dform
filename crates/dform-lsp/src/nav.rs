//! What the syntax tree says at a place: the module and policy
//! declarations and rule heads go-to-definition finds, and the contexts
//! completion reads (the resource or instance block the cursor is in).

use dform_core::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use rowan::TextSize;

/// The token at or just before byte `at`.
pub fn token_before(root: &SyntaxNode, at: usize) -> Option<SyntaxToken> {
    let at = TextSize::from(u32::try_from(at).ok()?);
    root.token_at_offset(at).left_biased()
}

/// The token under byte `at`, preferring a name to what abuts it.
pub fn token_at(root: &SyntaxNode, at: usize) -> Option<SyntaxToken> {
    let at = TextSize::from(u32::try_from(at).ok()?);
    let mut ts = root.token_at_offset(at);
    let (l, r) = (ts.next(), ts.next());
    match (l, r) {
        (Some(_), Some(r)) if r.kind() == SyntaxKind::IDENT => Some(r),
        (Some(l), _) => Some(l),
        _ => None,
    }
}

/// The dotted name after a node's keyword: `net.vpc` of `resource net.vpc
/// vpc {`, `network` of `instance network main {`.
pub fn name_after_keyword(node: &SyntaxNode) -> Option<String> {
    let mut out = String::new();
    let mut started = false;
    for t in node.children_with_tokens().filter_map(|e| e.into_token()) {
        match t.kind() {
            k if k.is_keyword() && !started => {}
            SyntaxKind::WHITESPACE | SyntaxKind::COMMENT if !started => {}
            SyntaxKind::IDENT | SyntaxKind::DOT => {
                started = true;
                out.push_str(t.text());
            }
            _ if started => break,
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The `IDENT` naming a declaration node (`module NAME`, `policy NAME`).
pub fn declared_name(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)
}

/// The predicate a rule or fact node states: its head call's name, or a
/// value rule's.
pub fn head_name(node: &SyntaxNode) -> Option<SyntaxToken> {
    match node.kind() {
        SyntaxKind::RULE | SyntaxKind::FACT => {
            let call = node.children().find(|c| c.kind() == SyntaxKind::CALL)?;
            let chain = call.children().find(|c| c.kind() == SyntaxKind::CHAIN)?;
            single_ident(&chain)
        }
        SyntaxKind::VALUE_RULE => declared_name(node),
        _ => None,
    }
}

/// A chain that is one name.
fn single_ident(chain: &SyntaxNode) -> Option<SyntaxToken> {
    let mut ts = chain
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia());
    let first = ts.next().filter(|t| t.kind() == SyntaxKind::IDENT)?;
    ts.next().is_none().then_some(first)
}

/// What a name at the cursor refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Module(String),
    Policy(String),
    Predicate(String),
}

/// What the name token `t` refers to, by where it stands.
pub fn reference(t: &SyntaxToken) -> Option<Ref> {
    if t.kind() != SyntaxKind::IDENT {
        return None;
    }
    let parent = t.parent()?;
    let name = t.text().to_string();
    match parent.kind() {
        SyntaxKind::INSTANCE if declared_name(&parent).as_ref() == Some(t) => {
            Some(Ref::Module(name))
        }
        SyntaxKind::APPLY => Some(Ref::Policy(name)),
        SyntaxKind::CHAIN => {
            let first = parent
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|x| !x.kind().is_trivia())?;
            if &first != t {
                return None;
            }
            let dotted = parent
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .any(|x| x.kind() == SyntaxKind::DOT || x.kind() == SyntaxKind::L_BRACKET)
                || parent.children().any(|c| c.kind() == SyntaxKind::INDEX);
            // `network.main.vpc`, `network[ia].vpc`: the module's; `p(x)`,
            // `zone_index[z]`: a predicate's (the caller checks which).
            if dotted {
                Some(Ref::Module(name))
            } else {
                Some(Ref::Predicate(name))
            }
        }
        _ => None,
    }
}

/// The declarations of `r` in the tree: `(start, end)` of each name.
pub fn definitions(root: &SyntaxNode, r: &Ref) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for n in root.descendants() {
        let t = match (r, n.kind()) {
            (Ref::Module(m), SyntaxKind::MODULE) => declared_name(&n).filter(|t| t.text() == m),
            (Ref::Policy(p), SyntaxKind::POLICY) => declared_name(&n).filter(|t| t.text() == p),
            (Ref::Predicate(p), SyntaxKind::RULE | SyntaxKind::FACT | SyntaxKind::VALUE_RULE) => {
                head_name(&n).filter(|t| t.text() == p)
            }
            _ => None,
        };
        if let Some(t) = t {
            let r = t.text_range();
            out.push((usize::from(r.start()), usize::from(r.end())));
        }
    }
    out
}

/// A module's declared inputs and outputs, each `(name, type text)`.
#[derive(Debug, Default)]
pub struct Interface {
    pub inputs: Vec<(String, String)>,
    pub outputs: Vec<(String, String)>,
}

pub fn module_interface(root: &SyntaxNode, module: &str) -> Option<Interface> {
    let m = root.descendants().find(|n| {
        n.kind() == SyntaxKind::MODULE && declared_name(n).is_some_and(|t| t.text() == module)
    })?;
    let block = m.children().find(|c| c.kind() == SyntaxKind::STMT_BLOCK)?;
    let mut out = Interface::default();
    for n in block.children() {
        let into = match n.kind() {
            SyntaxKind::INPUT => &mut out.inputs,
            // `output vpc: net.vpc` declares; `output vpc = vpc` defines.
            SyntaxKind::OUTPUT_DECL if n.children().any(|c| c.kind() == SyntaxKind::TYPE_EXPR) => {
                &mut out.outputs
            }
            _ => continue,
        };
        let Some(name) = declared_name(&n) else {
            continue;
        };
        let ty = n
            .children()
            .find(|c| c.kind() == SyntaxKind::TYPE_EXPR)
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        into.push((name.text().to_string(), ty));
    }
    Some(out)
}

/// The `contributes` patterns of the policy the node is in.
pub fn grants(node: &SyntaxNode) -> Vec<String> {
    let Some(policy) = node.ancestors().find(|n| n.kind() == SyntaxKind::POLICY) else {
        return Vec::new();
    };
    policy
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::CONTRIBUTES)
        .filter_map(|n| {
            n.children()
                .find(|c| c.kind() == SyntaxKind::CHAIN)
                .map(|c| c.text().to_string())
        })
        .collect()
}
