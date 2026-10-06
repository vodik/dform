//! What the syntax tree says at a place for completion: the resource or
//! instance block the cursor is in, the interface of the module a path
//! names (R-65). What a name denotes is `dform_core::names`'.

use dform_core::names::declared_name;
use dform_core::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use rowan::TextSize;

/// The token at or just before byte `at`.
pub fn token_before(root: &SyntaxNode, at: usize) -> Option<SyntaxToken> {
    let at = TextSize::from(u32::try_from(at).ok()?);
    root.token_at_offset(at).left_biased()
}

/// The dotted name after a node's keyword: `net.vpc` of `resource net.vpc
/// vpc {`, `network` of `resource network main {`, a component's.
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
            // `output vpc: net.vpc` declares; `output vpc = vpc` defines;
            // `output p` exports the relation `p` (R-55).
            SyntaxKind::OUTPUT_DECL
                if n.children().any(|c| c.kind() == SyntaxKind::TYPE_EXPR)
                    || !n.children_with_tokens().any(|e| e.kind() == SyntaxKind::EQ) =>
            {
                &mut out.outputs
            }
            _ => continue,
        };
        let Some(name) = declared_name(&n) else {
            continue;
        };
        let ty = match n.children().find(|c| c.kind() == SyntaxKind::TYPE_EXPR) {
            Some(t) => t.text().to_string(),
            None if n.kind() == SyntaxKind::OUTPUT_DECL => "relation".to_string(),
            None => String::new(),
        };
        into.push((name.text().to_string(), ty));
    }
    Some(out)
}
