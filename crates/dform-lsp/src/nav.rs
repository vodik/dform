//! What the syntax tree says at a place: the components, the files paths
//! name (R-65) and rule heads go-to-definition finds, and the contexts
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

/// The `IDENT` naming a declaration node (`component NAME`).
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
        SyntaxKind::LET => declared_name(node),
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
    /// A name in scope: a component, an instance, a used module
    /// (`network[t].vpc`, `blue.vpc`, `config.region`).
    Module(String),
    /// A path from the project root, up to the segment at the cursor:
    /// `modules.net` in `use modules.net` (R-65).
    Path(String),
    Predicate(String),
}

/// The path of a `use` or `instance` up to and including the token `t`,
/// when `t` is one of its segments.
fn path_to(parent: &SyntaxNode, t: &SyntaxToken) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let mut dot = true;
    for x in parent
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|x| !x.kind().is_trivia())
        .skip(1)
    {
        match x.kind() {
            SyntaxKind::DOT if !dot => dot = true,
            k if (k == SyntaxKind::IDENT || k.is_keyword()) && dot => {
                out.push(x.text().to_string());
                dot = false;
                if &x == t {
                    return Some(out.join("."));
                }
            }
            _ => return None,
        }
    }
    None
}

/// What the name token `t` refers to, by where it stands.
pub fn reference(t: &SyntaxToken) -> Option<Ref> {
    if t.kind() != SyntaxKind::IDENT {
        return None;
    }
    let parent = t.parent()?;
    let name = t.text().to_string();
    match parent.kind() {
        SyntaxKind::USE => path_to(&parent, t).map(Ref::Path),
        // `instance network blue`: a component the file declares, or one
        // a path names.
        SyntaxKind::INSTANCE => {
            let p = path_to(&parent, t)?;
            Some(match p.contains('.') {
                true => Ref::Path(p),
                false => Ref::Module(p),
            })
        }
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
            // `blue.vpc`, `network[ia].vpc`: a scope's; `p(x)`,
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

/// The declarations of `r` in the tree: `(start, end)` of each name. A
/// name in scope is declared by a `component`, or bound by an `instance`
/// or a `use`.
pub fn definitions(root: &SyntaxNode, r: &Ref) -> Vec<(usize, usize)> {
    use dform_core::syntax::resolve::{bound_token, component_name};
    let mut out = Vec::new();
    for n in root.descendants() {
        let t = match (r, n.kind()) {
            (Ref::Module(m), SyntaxKind::COMPONENT) if component_name(&n) == *m => {
                declared_name(&n)
            }
            (Ref::Module(m), SyntaxKind::INSTANCE | SyntaxKind::USE) => {
                bound_token(&n).filter(|t| t.text() == m)
            }
            (Ref::Predicate(p), SyntaxKind::RULE | SyntaxKind::FACT | SyntaxKind::LET) => {
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

/// The file a path names under `root` (R-65): `a.b` is `a/b.df`, or the
/// item `b` of `a.df`, with that item's name.
pub fn path_file(
    root: &std::path::Path,
    path: &str,
) -> Option<(std::path::PathBuf, Option<String>)> {
    let segs: Vec<&str> = path.split('.').collect();
    let file = |segs: &[&str]| {
        let mut f = root.to_path_buf();
        for s in segs {
            f.push(s);
        }
        f.set_extension("df");
        f
    };
    let whole = file(&segs);
    if whole.is_file() {
        return Some((whole, None));
    }
    let (last, init) = segs.split_last()?;
    let f = file(init);
    (!init.is_empty() && f.is_file()).then(|| (f, Some(last.to_string())))
}

/// A module's declared inputs and outputs, each `(name, type text)`.
#[derive(Debug, Default)]
pub struct Interface {
    pub inputs: Vec<(String, String)>,
    pub outputs: Vec<(String, String)>,
}

/// The interface of the component `module` a tree declares, `component
/// module { .. }`; with `module` `None`, the tree's own top level, a
/// component's file.
pub fn module_interface(root: &SyntaxNode, module: Option<&str>) -> Option<Interface> {
    let block = match module {
        Some(module) => root
            .descendants()
            .find(|n| {
                n.kind() == SyntaxKind::COMPONENT
                    && declared_name(n).is_some_and(|t| t.text() == module)
            })?
            .children()
            .find(|c| c.kind() == SyntaxKind::STMT_BLOCK)?,
        None => root.clone(),
    };
    let mut out = Interface::default();
    for n in block.children() {
        let into = match n.kind() {
            SyntaxKind::INPUT if !dform_core::syntax::resolve::is_key(&n) => &mut out.inputs,
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
