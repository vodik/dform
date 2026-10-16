//! Queries over the scope tree (R-211 step 12): what a name written at a
//! place reaches, the scope a place is in, what a copy's component gives
//! its user, a let's parameters, every read of a declaration. The front
//! end resolves by the same walks (`Scopes::chain`, `Scopes::around`);
//! these answer a place rather than a statement being lowered, for the
//! language server and `why`.

use super::{Decl, DeclKind, DeclRef, ScopeId, Scopes};
use crate::ast::Span;
use crate::program::node::Name;
use crate::program::{ExprKind, Program};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

impl Scopes {
    /// The innermost scope written at byte `offset` of `file`: the body of
    /// the component whose statement holds it, else the file's top level.
    pub fn at(&self, file: u32, offset: u32) -> Option<ScopeId> {
        let mut s = self.of_file(file)?;
        while let Some(inner) = self.component_at(s, file, offset) {
            s = inner;
        }
        Some(s)
    }

    /// The body of the component written directly in `s` whose statement
    /// holds byte `offset` of `file`.
    fn component_at(&self, s: ScopeId, file: u32, offset: u32) -> Option<ScopeId> {
        self.names[self.declaring(s)].iter().find_map(|(_, decls)| {
            decls.iter().find_map(|d| {
                let DeclKind::Component(_) = d.kind else {
                    return None;
                };
                let range = d.node.text_range();
                let holds = range.start() <= offset.into() && offset < u32::from(range.end());
                let body = self.of_block(file, range.start().into())?;
                (holds && self[body].parent == Some(s)).then_some(body)
            })
        })
    }

    /// What a read of `name` written in `s` reaches: the innermost scope
    /// around `s` with a declaration of it `pick` takes, and the first
    /// such. A module's file has no scope around it (R-205).
    pub fn reaching(
        &self,
        s: ScopeId,
        name: &str,
        pick: impl Fn(&DeclKind) -> bool,
    ) -> Option<(ScopeId, &Decl)> {
        self.chain(s).into_iter().find_map(|at| {
            let d = self.names[at].of(name).iter().find(|d| pick(&d.kind))?;
            Some((at, d))
        })
    }

    /// What `super.name` (`supers` times `super`) written in `s` reaches:
    /// the read made from the scope around each enclosing component's
    /// body in turn (R-186). `None` past the outermost component.
    pub fn past(
        &self,
        s: ScopeId,
        supers: usize,
        name: &str,
        pick: impl Fn(&DeclKind) -> bool,
    ) -> Option<(ScopeId, &Decl)> {
        let mut at = s;
        for _ in 0..supers {
            let body = self.body(at);
            if !self.is_component(body) {
                return None;
            }
            at = self.around(body);
        }
        self.reaching(at, name, pick)
    }

    /// Every declaration of every scope, with its scope and its name, in
    /// the order the scopes were made, each scope's by name.
    pub fn every(&self) -> impl Iterator<Item = (ScopeId, &str, &Decl)> {
        self.ids().flat_map(move |s| {
            self.names[s]
                .iter()
                .flat_map(move |(n, ds)| ds.iter().map(move |d| (s, n.as_str(), d)))
        })
    }

    /// The declaration the statement `stmt` written in `s` makes that
    /// `pick` takes (a copy also makes its component readable by its last
    /// segment): its name and its declaration.
    pub fn made_by(
        &self,
        s: ScopeId,
        stmt: &SyntaxNode,
        pick: impl Fn(&DeclKind) -> bool,
    ) -> Option<(&str, &Decl)> {
        self.names[self.declaring(s)].iter().find_map(|(n, ds)| {
            ds.iter()
                .find(|d| d.node == *stmt && pick(&d.kind))
                .map(|d| (n.as_str(), d))
        })
    }

    /// The path of a component whose last segment is `name`, when the
    /// program declares one.
    pub fn component_named(&self, name: &str) -> Option<&Name> {
        self.every().find_map(|(_, n, d)| match &d.kind {
            DeclKind::Component(p) if n == name => Some(p),
            _ => None,
        })
    }

    /// Whether any scope makes `name` a relation its statements define
    /// (`defines_relation`), or the program declares an extern of it.
    pub fn is_relation(&self, name: &str) -> bool {
        self.externs.contains_key(name)
            || self
                .every()
                .any(|(_, n, d)| n == name && defines_relation(&d.kind))
    }

    /// The first of `scopes` that is a module's or a component's body and
    /// defines the relation `name`: the scope that keeps it private.
    pub fn private_relation(
        &self,
        scopes: impl IntoIterator<Item = ScopeId>,
        name: &str,
    ) -> Option<ScopeId> {
        scopes.into_iter().find(|s| {
            self.definition(*s).is_some()
                && self.names[*s]
                    .of(name)
                    .iter()
                    .any(|d| defines_relation(&d.kind))
        })
    }

    /// The names that bind the definition at `path`, in the order met:
    /// each copy's of the component, else each `use`'s of the module.
    pub fn bound_to(&self, path: &str, component: bool) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for (_, n, d) in self.every() {
            let of = match &d.kind {
                DeclKind::Copy {
                    component: Some(c), ..
                } if component => c,
                DeclKind::Use { module, .. } if !component => module,
                _ => continue,
            };
            if of == path && !out.contains(&n) {
                out.push(n);
            }
        }
        out
    }

    /// The declaration a read names.
    pub fn declaration(&self, d: &DeclRef) -> Option<&Decl> {
        self.names.get(d.scope)?.of(&d.name).get(d.at)
    }

    /// The body of the component the copy `name` reached from `s` is of.
    pub fn copied_body(&self, s: ScopeId, name: &str) -> Option<ScopeId> {
        let copy = |k: &DeclKind| {
            matches!(
                k,
                DeclKind::Copy {
                    component: Some(_),
                    ..
                }
            )
        };
        let (_, d) = self.reaching(s, name, copy)?;
        match &d.kind {
            DeclKind::Copy {
                component: Some(c), ..
            } => self.by_path(c),
            _ => None,
        }
    }

    /// The outputs the definition whose body is `body` gives its user (an
    /// instance's outputs): each name and its declarations, by name.
    pub fn outputs(&self, body: ScopeId) -> Vec<(&str, &Decl)> {
        self.names[body]
            .iter()
            .filter_map(|(n, ds)| {
                let d = ds
                    .iter()
                    .find(|d| matches!(d.kind, DeclKind::Output { .. }))?;
                Some((n.as_str(), d))
            })
            .collect()
    }

    /// Where the parameter `param` of the let with parameters `function`
    /// a read in `s` reaches is declared: its name in the let's head.
    pub fn parameter(&self, s: ScopeId, function: &str, param: &str) -> Option<SyntaxToken> {
        let fun = |k: &DeclKind| matches!(k, DeclKind::LetFn { .. });
        let (_, d) = self.reaching(s, function, fun)?;
        parameters(&d.node).into_iter().find(|t| t.text() == param)
    }
}

/// Whether a declaration of the kind makes its name a relation the
/// scope's own statements define or declare: a rule's head, a fact, a
/// `decl`, an input relation.
pub fn defines_relation(k: &DeclKind) -> bool {
    matches!(
        k,
        DeclKind::Rule { .. } | DeclKind::Decl { .. } | DeclKind::RelationInput { .. }
    )
}

/// The parameters of a `let` statement, each its name's token in the
/// let's head (`a` and `b` of `let f(a, b: int = 1) = ..`); none for a let
/// without them.
pub fn parameters(let_stmt: &SyntaxNode) -> Vec<SyntaxToken> {
    let Some(list) = let_stmt.children().find(|c| c.kind() == SyntaxKind::PARAMS) else {
        return Vec::new();
    };
    list.children()
        .filter(|c| c.kind() == SyntaxKind::PARAM)
        .filter_map(|p| {
            p.children_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| t.kind() == SyntaxKind::IDENT)
        })
        .collect()
}

impl Program {
    /// Every read node of the declaration `d`, where each is written, in
    /// the order the nodes were made. A statement reads a declaration
    /// once: a second read of it in the same statement (`${tag}` and
    /// `${super.tag}` naming one input) shares the first's node, so these
    /// are the places each statement first reads it, not every place.
    pub fn reads_of(&self, d: &DeclRef) -> Vec<Span> {
        self.exprs
            .values()
            .filter(|e| match &e.kind {
                ExprKind::Value { decl, .. } => decl == d,
                ExprKind::Deployed { stack, .. } => stack == d,
                _ => false,
            })
            .map(|e| e.span)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::resolve::{Mode, Unit, lower_stack};

    /// The nodes `files` build to: the first the stack's file, the rest
    /// modules by their paths; with each file's source id.
    fn program(files: &[(&str, Option<&str>, &str)]) -> (Program, Vec<u32>) {
        let units: Vec<Unit> = files
            .iter()
            .map(|(name, path, src)| {
                let parse = crate::syntax::parser::parse(src);
                assert!(parse.errors.is_empty(), "{:?}", parse.errors);
                Unit {
                    file: crate::diag::add_source(name, src),
                    root: parse.syntax(),
                    path: path.map(str::to_string),
                    project: false,
                }
            })
            .collect();
        let ids = units.iter().map(|u| u.file).collect();
        let lowered = lower_stack(&units, &[0], true, Mode::Program, None, &[]).unwrap();
        (Program::clone(lowered.nodes.as_ref().unwrap()), ids)
    }

    fn offset(src: &str, needle: &str) -> u32 {
        src.find(needle).unwrap() as u32
    }

    const STACK: &str = r#"input tag: string = "s"
let base = "b"
component outer {
  input tag: string = "o"
  output name = tag
  component inner {
    resource net.vpc v { cidr = "${tag}-${super.tag}-${super.super.tag}-${base}" }
  }
  resource inner i {}
}
resource outer o {}
let cidr(n, m: int = 2) = "10.${n}.${m}.0/24"
let x = cidr(1)
"#;

    #[test]
    fn a_place_is_in_its_innermost_component() {
        let (p, ids) = program(&[("q.df", None, STACK)]);
        let s = &p.scopes;
        let file = s.of_file(ids[0]).unwrap();
        let outer = s.by_path("outer").unwrap();
        let inner = s.by_path("inner").unwrap();
        assert_eq!(s.at(ids[0], 0), Some(file));
        assert_eq!(s.at(ids[0], offset(STACK, "output name")), Some(outer));
        assert_eq!(s.at(ids[0], offset(STACK, "resource net.vpc")), Some(inner));
        assert_eq!(s.at(ids[0], offset(STACK, "resource inner i")), Some(outer));
        assert_eq!(s.at(ids[0], offset(STACK, "resource outer o")), Some(file));
    }

    #[test]
    fn a_read_reaches_the_innermost_declaration_and_super_the_one_around() {
        let (p, _) = program(&[("q.df", None, STACK)]);
        let s = &p.scopes;
        let inner = s.by_path("inner").unwrap();
        let outer = s.by_path("outer").unwrap();
        let value = DeclKind::is_value;
        assert_eq!(s.reaching(inner, "tag", value).map(|x| x.0), Some(outer));
        assert_eq!(s.reaching(inner, "base", value).map(|x| x.0), Some(s.root));
        assert_eq!(s.past(inner, 1, "tag", value).map(|x| x.0), Some(outer));
        assert_eq!(s.past(inner, 2, "tag", value).map(|x| x.0), Some(s.root));
        assert!(s.past(inner, 3, "tag", value).is_none());
        assert!(s.reaching(inner, "nothing", value).is_none());
    }

    #[test]
    fn a_module_reads_nothing_of_its_user() {
        let net = "input region: string\ncomponent vpc {\n  output id = region\n}\n";
        let stack = "let region = \"eu\"\nuse lan { region = region }\nresource lan.vpc v {}\n";
        let (p, ids) = program(&[("s.df", None, stack), ("lan.df", Some("lan"), net)]);
        let s = &p.scopes;
        let vpc = s.at(ids[1], offset(net, "output id")).unwrap();
        assert_eq!(s.by_path("lan.vpc"), Some(vpc));
        let module = s.by_path("lan").unwrap();
        let (at, d) = s.reaching(vpc, "region", DeclKind::is_value).unwrap();
        assert_eq!((at, &d.kind), (module, &DeclKind::Input));
        // The copy's component and what it gives its user.
        let file = s.of_file(ids[0]).unwrap();
        assert_eq!(s.copied_body(file, "v"), Some(vpc));
        let outs: Vec<&str> = s.outputs(vpc).into_iter().map(|(n, _)| n).collect();
        assert_eq!(outs, ["id"]);
    }

    #[test]
    fn a_statement_makes_its_declaration() {
        let (p, ids) = program(&[("q.df", None, STACK)]);
        let s = &p.scopes;
        let outer = s.by_path("outer").unwrap();
        let (_, _, copy) = s
            .every()
            .find(|(_, n, d)| *n == "i" && matches!(d.kind, DeclKind::Copy { .. }))
            .unwrap();
        let at = s.at(ids[0], offset(STACK, "resource inner i")).unwrap();
        assert_eq!(at, outer);
        let copies = |k: &DeclKind| matches!(k, DeclKind::Copy { .. });
        let (name, made) = s.made_by(at, &copy.node, copies).unwrap();
        assert_eq!((name, made.node.clone()), ("i", copy.node.clone()));
        // The same statement makes `inner` readable by its last segment.
        let copied = |k: &DeclKind| matches!(k, DeclKind::Copied(_));
        assert_eq!(
            s.made_by(at, &copy.node, copied).map(|x| x.0),
            Some("inner")
        );
        assert!(s.every().any(|(sc, n, _)| sc == s.root && n == "cidr"));
        assert_eq!(
            s.component_named("inner").map(String::as_str),
            Some("inner")
        );
        assert_eq!(s.bound_to("inner", true), ["i"]);
        assert_eq!(s.bound_to("outer", true), ["o"]);
        assert!(!s.is_relation("cidr") && !s.is_relation("tag"));
    }

    #[test]
    fn a_lets_parameters_are_declared_in_its_head() {
        let (p, ids) = program(&[("q.df", None, STACK)]);
        let s = &p.scopes;
        let file = s.of_file(ids[0]).unwrap();
        let n = s.parameter(file, "cidr", "n").unwrap();
        assert_eq!(u32::from(n.text_range().start()), offset(STACK, "n, m"));
        let m = s.parameter(file, "cidr", "m").unwrap();
        assert_eq!(u32::from(m.text_range().start()), offset(STACK, "m: int"));
        assert!(s.parameter(file, "cidr", "k").is_none());
        assert!(s.parameter(file, "x", "n").is_none());
    }

    /// A read of a name a scope declares as an output and a `let` is the
    /// `let`'s: the declaration it names is the value's.
    #[test]
    fn a_read_names_the_value_an_output_shares_its_name_with() {
        let src = "output k = \"o\"\nlet k = \"v\"\noutput m = k\n";
        let (p, _) = program(&[("k.df", None, src)]);
        let read = p
            .exprs
            .values()
            .find_map(|e| match &e.kind {
                ExprKind::Value { decl, .. } => Some(decl.clone()),
                _ => None,
            })
            .expect("the read of k");
        let d = p.scopes.declaration(&read).expect("its declaration");
        assert_eq!(d.kind, DeclKind::Let);
    }

    #[test]
    fn every_read_of_a_declaration() {
        let (p, ids) = program(&[("q.df", None, STACK)]);
        let outer = p.scopes.by_path("outer").unwrap();
        let tag = DeclRef {
            scope: outer,
            name: "tag".into(),
            at: 0,
        };
        assert!(p.scopes.declaration(&tag).is_some());
        let mut starts: Vec<u32> = p.reads_of(&tag).iter().map(|s| s.start).collect();
        starts.sort();
        starts.dedup();
        // `output name = tag` and inner's `${tag}`; the statement's
        // `${super.tag}` reads the same and shares the `${tag}` node.
        let want = [
            offset(STACK, "tag\n  component"),
            offset(STACK, "tag}-${super.tag}"),
        ];
        assert_eq!(starts, want);
        assert!(p.reads_of(&tag).iter().all(|s| s.file == ids[0]));
    }
}
