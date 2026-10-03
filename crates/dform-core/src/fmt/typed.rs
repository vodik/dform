//! The typed-position normal form (R-52, amended 2026-10-02): where a
//! position has a type the compiler reads a literal as, the literal is
//! written in its shortest spelling. A string in a `bytes`, `cpu` or
//! `duration` position that reads as one loses its quotes (`"2Gi"` is
//! `2Gi`, `"500m"` `500m`); in an `inet`, `ip` or `time` position a
//! constructor of a string is dropped (`inet("10.0.0.0/16")` is
//! `"10.0.0.0/16"`). A literal that does not read as its type is left for
//! the compiler to report.
//!
//! The positions, each one the compiler reads a literal at as its type
//! (`types::literal`, `types::read`):
//! - a schema attribute in a resource's block or a `set` (`set c.resources
//!   .limits = ..` through a variable bound over a keyed list's elements),
//!   at its own path or nested in an object or a list value;
//! - an input's default, an object input's field's;
//! - an `instance`'s or a `use`'s entry for its component's or module's
//!   input;
//! - a function's typed parameter (std's signatures).
//!
//! Only inside a project, whose providers' schemas fmt reads from their
//! schema files (the mock's, built in or the project's own), starting none.

use crate::functions;
use crate::project::Project;
use crate::schema::Schema;
use crate::syntax::SyntaxKind::*;
use crate::syntax::{SyntaxNode, SyntaxToken};
use crate::types::Ty;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What fmt knows of a project's types.
pub struct Typing {
    schema: Schema,
    root: PathBuf,
}

impl Typing {
    /// The project's typing: its providers' schemas (the manifest's and
    /// those its files' `provider` statements name) that a schema file
    /// gives, read without starting a provider. A provider that is an
    /// executable types nothing here.
    pub fn of_project(project: &Project) -> Typing {
        let mut names: Vec<String> = project.manifest.providers.keys().cloned().collect();
        for f in crate::project::df_files(project) {
            let path = if f.is_absolute() {
                f
            } else {
                project.root.join(f)
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let tree = crate::syntax::parser::parse(&text).syntax();
            for p in tree.descendants().filter(|n| n.kind() == PROVIDER) {
                if let Some(t) = words(&p).nth(1) {
                    names.push(t.text().to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        let mut schema = Schema::default();
        for n in names {
            let src = project
                .manifest
                .provider_source(&n)
                .unwrap_or_else(|| n.clone());
            if let Some(s) = schema_of(&project.root, &src)
                && let Ok(merged) = schema.clone().merge(s)
            {
                schema = merged;
            }
        }
        Typing {
            schema,
            root: project.root.clone(),
        }
    }
}

/// The schema a provider's source gives without starting it: a schema
/// file, a directory's `schema.df`, the project's `providers/NAME`, a
/// built-in mock schema.
fn schema_of(root: &Path, src: &str) -> Option<Schema> {
    let path = root.join(src);
    let file = if src.contains('/') || src.ends_with(".df") {
        if path.is_dir() {
            if crate::plugin::source::plugin_in(&path).is_some() {
                return None;
            }
            path.join("schema.df")
        } else {
            path
        }
    } else {
        root.join("providers").join(src).join("schema.df")
    };
    if file.extension().is_some_and(|e| e == "df") && file.is_file() {
        return Schema::load(&file).ok();
    }
    let text = crate::schema::builtin(src)?;
    Schema::parse(text, src).ok()
}

fn words(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == IDENT || t.kind().is_keyword())
}

fn text(n: &SyntaxNode) -> String {
    n.text().to_string().trim().to_string()
}

/// A dotted name's text without its spaces: `k8s.deployment`.
fn dotted(n: &SyntaxNode) -> String {
    n.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
        .map(|t| t.text().to_string())
        .collect()
}

/// The text of a string literal with no hole and no escape, unquoted.
fn plain_string(n: &SyntaxNode) -> Option<String> {
    if n.kind() != LITERAL {
        return None;
    }
    let t = n.first_token().filter(|t| t.kind() == STRING)?;
    let inner = t.text().strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains("${") && !inner.contains('\\') && !inner.contains('"'))
        .then(|| inner.to_string())
}

/// Whether the quantity text `t` reads the same unquoted, as a `dim`: it
/// is one quantity token, and its reading as written (or, ambiguous, the
/// position's) is the string's.
fn unquotes(dim: &str, t: &str) -> bool {
    use crate::quantity::{self, Dim, Literal};
    let toks = crate::lexer::lex(t);
    let [tok] = toks.as_slice() else {
        return false;
    };
    if tok.kind != QUANTITY || tok.start != 0 || tok.end != t.len() {
        return false;
    }
    let Some(d) = Dim::parse(dim) else {
        return false;
    };
    let Ok(as_string) = quantity::read(d, t) else {
        return false;
    };
    match quantity::literal(t) {
        Ok(Literal::Known(q)) => q == as_string,
        Ok(Literal::Ambiguous) => true,
        Err(_) => false,
    }
}

/// Whether `s`, the argument of the constructor `ty`, reads as one and
/// prints back as written: the string is the same value in the position.
fn canonical(ty: &str, s: &str) -> bool {
    use crate::value;
    match ty {
        "inet" => value::parse_ipnet(s).is_some_and(|(a, p)| value::ipnet_to_string(a, p) == s),
        "ip" => value::ipv4_to_u32(s).is_some_and(|a| value::u32_to_ipv4(a) == s),
        "time" => crate::time::Time::parse(s).is_ok_and(|t| t.to_string() == s),
        _ => false,
    }
}

struct Ctx<'a> {
    typing: &'a Typing,
    edits: Vec<(usize, usize, String)>,
    /// Every file's tree read for a module's inputs, by path.
    modules: BTreeMap<PathBuf, Option<SyntaxNode>>,
}

impl Ctx<'_> {
    fn put(&mut self, n: &SyntaxNode, s: String) {
        let r = n.text_range();
        self.edits.push((r.start().into(), r.end().into(), s));
    }

    /// The literal `term` in a position of type `ty`, in its shortest
    /// spelling.
    fn literal(&mut self, term: &SyntaxNode, ty: &Ty) {
        match ty {
            Ty::Secret(t) => self.literal(term, t),
            Ty::List(t) if term.kind() == LIST => {
                for el in term.children() {
                    self.literal(&el, t);
                }
            }
            Ty::Scalar(s) if matches!(s.as_str(), "bytes" | "cpu" | "duration") => {
                if let Some(t) = plain_string(term)
                    && unquotes(s, &t)
                {
                    self.put(term, t);
                }
            }
            Ty::Scalar(s) if matches!(s.as_str(), "inet" | "ip" | "time") => {
                if term.kind() != CALL {
                    return;
                }
                let Some(callee) = term.children().find(|c| c.kind() == CHAIN) else {
                    return;
                };
                let Some(args) = term.children().find(|c| c.kind() == ARG_LIST) else {
                    return;
                };
                let args: Vec<SyntaxNode> = args.children().collect();
                if dotted(&callee) != *s {
                    return;
                }
                if let [a] = args.as_slice()
                    && let Some(t) = plain_string(a)
                    && canonical(s, &t)
                {
                    self.put(term, text(a));
                }
            }
            _ => {}
        }
    }

    /// `term` at `path` of a `typ`: its attribute's type, else the
    /// attributes nested in it (an object's fields, a list's elements).
    fn attr(&mut self, typ: &str, path: &str, term: &SyntaxNode) {
        if let Some(spec) = self.typing.schema.attr(typ, path) {
            let ty = Ty::parse(&spec.ty);
            if !matches!(ty, Ty::Any) {
                self.literal(term, &ty);
                return;
            }
        }
        match term.kind() {
            OBJECT => {
                for f in term.children() {
                    let Some(key) = f.first_token() else { continue };
                    let Some(v) = f.children().next() else {
                        continue;
                    };
                    let k = key.text().trim_matches('"');
                    self.attr(typ, &format!("{path}.{k}"), &v);
                }
            }
            LIST => {
                for el in term.children() {
                    self.attr(typ, path, &el);
                }
            }
            _ => {}
        }
    }

    fn known_type(&self, typ: &str) -> bool {
        self.typing.schema.attrs.keys().any(|(t, _)| t == typ)
    }

    /// `resource T n { path = term .. }`.
    fn resource(&mut self, r: &SyntaxNode) {
        let (Some(typ), Some(block)) = (
            type_of_header(&header(r)),
            r.children().find(|c| c.kind() == BLOCK),
        ) else {
            return;
        };
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let Some(path) = a.children().find(|c| c.kind() == BLOCK_PATH) else {
                continue;
            };
            let Some(value) = a.children().filter(|c| c.kind() != BLOCK_PATH).last() else {
                continue;
            };
            let p = dotted(&path);
            if !p.contains(['[', '"']) {
                self.attr(&typ, &p, &value);
            }
        }
    }

    /// `set target = term [where B]`, `set { target = term .. } [where B]`:
    /// a target that is a resource's attribute, by its name in the file,
    /// `T[e]`, or a variable the body binds over a type or an attribute.
    fn set(&mut self, s: &SyntaxNode, resources: &BTreeMap<String, String>) {
        let mut vars: BTreeMap<String, (String, String)> = BTreeMap::new();
        if let Some(body) = s.children().find(|c| c.kind() == BODY) {
            // To a fixpoint: a variable over another's attribute may come
            // first.
            for _ in 0..4 {
                for l in body.children().filter(|l| l.kind() == LIT_IN) {
                    let sides: Vec<SyntaxNode> = l.children().collect();
                    let [v, over] = sides.as_slice() else {
                        continue;
                    };
                    if v.kind() != CHAIN || over.kind() != CHAIN || dotted(v).contains('.') {
                        continue;
                    }
                    if let Some(at) = self.target(over, &vars, resources) {
                        vars.insert(dotted(v), at);
                    }
                }
            }
        }
        let assigns: Vec<SyntaxNode> = match s.children().find(|c| c.kind() == BLOCK) {
            Some(b) => b.children().filter(|c| c.kind() == ASSIGN).collect(),
            None => vec![s.clone()],
        };
        for a in assigns {
            let parts: Vec<SyntaxNode> = a.children().collect();
            let Some(lhs) = parts.iter().find(|c| c.kind() == CHAIN) else {
                continue;
            };
            let Some(rhs) = parts
                .iter()
                .skip_while(|c| *c != lhs)
                .nth(1)
                .filter(|c| c.kind() != BODY)
            else {
                continue;
            };
            if let Some((typ, path)) = self.target(lhs, &vars, resources)
                && !path.is_empty()
            {
                self.attr(&typ, &path, rhs);
            }
        }
    }

    /// What a chain names: a resource type and an attribute path in it
    /// (an element of a list being the list's path), or `None`.
    fn target(
        &self,
        chain: &SyntaxNode,
        vars: &BTreeMap<String, (String, String)>,
        resources: &BTreeMap<String, String>,
    ) -> Option<(String, String)> {
        // Segments: names, and `[..]` lookups (an index into a list is its
        // element, at the list's path).
        let mut segs: Vec<Option<String>> = Vec::new();
        for e in chain.children_with_tokens() {
            match e {
                rowan::NodeOrToken::Token(t) if t.kind() == IDENT || t.kind().is_keyword() => {
                    segs.push(Some(t.text().to_string()))
                }
                rowan::NodeOrToken::Token(t) if t.kind() == STRING => {
                    segs.push(Some(t.text().trim_matches('"').to_string()))
                }
                rowan::NodeOrToken::Node(n) if n.kind() == INDEX => segs.push(None),
                _ => {}
            }
        }
        let head = segs.first()?.clone()?;
        let (typ, base, rest) = if let Some((t, p)) = vars.get(&head) {
            (t.clone(), p.clone(), &segs[1..])
        } else if let Some(t) = resources.get(&head) {
            (t.clone(), String::new(), &segs[1..])
        } else {
            // `T` or `T[e]`: the longest dotted prefix the schema types.
            let names: Vec<&String> = segs.iter().map_while(|s| s.as_ref()).collect();
            let n = (1..=names.len()).rev().find(|&n| {
                let t: Vec<&str> = names[..n].iter().map(|s| s.as_str()).collect();
                self.known_type(&t.join("."))
            })?;
            let t: Vec<&str> = names[..n].iter().map(|s| s.as_str()).collect();
            let rest = &segs[n..];
            // `T[e]` is a resource; a bare `T` is the type, read by `in`.
            let rest = match rest.first() {
                Some(None) => &rest[1..],
                _ => rest,
            };
            (t.join("."), String::new(), rest)
        };
        let mut path = base;
        for s in rest.iter().flatten() {
            if !path.is_empty() {
                path.push('.');
            }
            path.push_str(s);
        }
        Some((typ, path))
    }

    /// `input k: T = term`, `input k { f: T = term .. }`: the defaults.
    fn input(&mut self, n: &SyntaxNode) {
        if let Some(ty) = n.children().find(|c| c.kind() == TYPE_EXPR)
            && let Some(d) = n
                .children()
                .filter(|c| c.kind() != TYPE_EXPR)
                .find(|c| c.kind() != REFINEMENT && c.kind() != ATTR_DECL && c.kind() != BIND_ARG)
        {
            self.literal(&d, &Ty::parse(&dotted(&ty)));
        }
        for f in n.descendants().filter(|c| c.kind() == ATTR_DECL) {
            let Some(ty) = f.children().find(|c| c.kind() == TYPE_EXPR) else {
                continue;
            };
            if let Some(d) = f
                .children()
                .find(|c| !matches!(c.kind(), TYPE_EXPR | BLOCK_PATH | REFINEMENT | ATTR_DECL))
            {
                self.literal(&d, &Ty::parse(&dotted(&ty)));
            }
        }
    }

    /// An `instance`'s or a `use`'s entries: its component's or module's
    /// inputs' types.
    fn copy(&mut self, n: &SyntaxNode, file: &SyntaxNode) {
        let Some(block) = n.children().find(|c| c.kind() == BLOCK) else {
            return;
        };
        let path: Vec<String> = n
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| !t.kind().is_trivia())
            .skip(1)
            .take_while(|t| t.kind() == IDENT || t.kind().is_keyword() || t.kind() == DOT)
            .filter(|t| t.kind() != DOT)
            .map(|t| t.text().to_string())
            .collect();
        let inputs = match n.kind() {
            INSTANCE => {
                // `instance a.b.comp name`: the component `comp` of the
                // module `a.b`, or of this file.
                let Some((comp, module)) = path[..path.len().saturating_sub(1)].split_last() else {
                    return;
                };
                let tree = if module.is_empty() {
                    Some(file.clone())
                } else {
                    self.module(module)
                };
                let Some(tree) = tree else { return };
                let Some(c) = tree.children().find(|c| {
                    c.kind() == COMPONENT && words(c).nth(1).is_some_and(|w| w.text() == comp)
                }) else {
                    return;
                };
                let Some(stmts) = c.children().find(|x| x.kind() == STMT_BLOCK) else {
                    return;
                };
                inputs_of(&stmts)
            }
            _ => {
                // `use a.b [as n]`: the module file's inputs.
                let module: Vec<String> = path.iter().take_while(|w| *w != "as").cloned().collect();
                let Some(tree) = self.module(&module) else {
                    return;
                };
                inputs_of(&tree)
            }
        };
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let Some(p) = a.children().find(|c| c.kind() == BLOCK_PATH) else {
                continue;
            };
            let Some(v) = a.children().filter(|c| c.kind() != BLOCK_PATH).last() else {
                continue;
            };
            if let Some(ty) = inputs.get(&dotted(&p)) {
                self.literal(&v, ty);
            }
        }
    }

    /// The tree of the module `path` (`modules.net`: modules/net.df under
    /// the project root).
    fn module(&mut self, path: &[String]) -> Option<SyntaxNode> {
        let file = self.typing.root.join(format!("{}.df", path.join("/")));
        self.modules
            .entry(file.clone())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(&file).ok()?;
                Some(crate::syntax::parser::parse(&text).syntax())
            })
            .clone()
    }

    /// A function's typed parameters: what the resolver reads a literal
    /// argument as (`resolve::args`: a scalar parameter not a string).
    fn call(&mut self, c: &SyntaxNode) {
        if c.ancestors().any(|a| a.kind() == REFINEMENT) {
            return;
        }
        let Some(callee) = c.children().find(|x| x.kind() == CHAIN) else {
            return;
        };
        let name = dotted(&callee);
        let Some(f) = functions::get(&name) else {
            return;
        };
        if matches!(
            name.as_str(),
            "bytes" | "cpu" | "duration" | "time" | "inet" | "ip"
        ) {
            return;
        }
        let Some(args) = c.children().find(|x| x.kind() == ARG_LIST) else {
            return;
        };
        for (i, a) in args.children().enumerate() {
            if a.kind() == NAMED_ARG {
                continue;
            }
            let p = f
                .params
                .get(i)
                .or(if f.variadic { f.params.last() } else { None });
            let Some(p) = p.filter(|p| p.ty != "string") else {
                continue;
            };
            let ty = Ty::parse(&p.ty);
            if matches!(ty, Ty::Scalar(_)) {
                self.literal(&a, &ty);
            }
        }
    }
}

/// A resource statement's header tokens: `resource T.y.p.e NAME [@rank]`.
fn header(r: &SyntaxNode) -> Vec<SyntaxToken> {
    r.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
        .collect()
}

/// `resource T.y.p.e NAME`: the type, all but the name.
fn type_of_header(ws: &[SyntaxToken]) -> Option<String> {
    let parts: Vec<&SyntaxToken> = ws.iter().skip(1).filter(|t| t.kind() != RANK).collect();
    let (_, typ) = parts.split_last()?;
    let s: String = typ.iter().map(|t| t.text()).collect();
    (!s.is_empty()).then_some(s)
}

/// The scalar inputs a module's or component's statements declare, by
/// name, and an object input's fields by `k.f`.
fn inputs_of(stmts: &SyntaxNode) -> BTreeMap<String, Ty> {
    let mut out = BTreeMap::new();
    for i in stmts.children().filter(|c| c.kind() == INPUT) {
        let Some(name) = words(&i).nth(1) else {
            continue;
        };
        if let Some(ty) = i.children().find(|c| c.kind() == TYPE_EXPR) {
            out.insert(name.text().to_string(), Ty::parse(&dotted(&ty)));
        }
        for f in i.children().filter(|c| c.kind() == ATTR_DECL) {
            let (Some(p), Some(ty)) = (
                f.children().find(|c| c.kind() == BLOCK_PATH),
                f.children().find(|c| c.kind() == TYPE_EXPR),
            ) else {
                continue;
            };
            out.insert(
                format!("{}.{}", name.text(), dotted(&p)),
                Ty::parse(&dotted(&ty)),
            );
        }
    }
    out
}

/// The source with its typed literals in their shortest spelling, or
/// `None` when they are.
pub fn normalize(root: &SyntaxNode, src: &str, typing: &Typing) -> Option<String> {
    let mut c = Ctx {
        typing,
        edits: Vec::new(),
        modules: BTreeMap::new(),
    };
    // The file's resources by name: `set main.tags = ..`.
    let mut resources = BTreeMap::new();
    for r in root.descendants().filter(|n| n.kind() == RESOURCE) {
        let ws = header(&r);
        if let (Some(typ), Some(name)) =
            (type_of_header(&ws), ws.iter().rfind(|t| t.kind() != RANK))
            && name.kind() == IDENT
        {
            resources.insert(name.text().to_string(), typ);
        }
    }
    for n in root.descendants() {
        match n.kind() {
            RESOURCE => c.resource(&n),
            SET => c.set(&n, &resources),
            INPUT => c.input(&n),
            INSTANCE | USE => c.copy(&n, root),
            CALL => c.call(&n),
            _ => {}
        }
    }
    super::normal::apply(src, c.edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typing() -> Typing {
        let mut schema = Schema::default();
        for n in ["k8s", "fake"] {
            let s = Schema::parse(crate::schema::builtin(n).unwrap(), n).unwrap();
            schema = schema.merge(s).unwrap();
        }
        Typing {
            schema,
            root: PathBuf::from("/nonexistent"),
        }
    }

    /// `src` formatted in a project, and outside one, where it must be
    /// as written (each test's input is otherwise formatted).
    fn fmt(src: &str) -> String {
        let p = crate::syntax::parser::parse(src);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert_eq!(super::super::format(&p.syntax()), src, "outside a project");
        super::super::format_in(&p.syntax(), Some(&typing()))
    }

    /// A string in a quantity's position loses its quotes, at the
    /// attribute's own path or nested in an object or a list's element;
    /// one that does not read as the type, or in a string's position,
    /// stays.
    #[test]
    fn a_quantity_string_in_a_quantity_position_loses_its_quotes() {
        let src = "resource k8s.deployment d {\n  metadata.name = \"2Gi\"\n  \
                   spec.template.spec.containers = [{\n    name: \"a\",\n    \
                   resources: { limits: { memory: \"2Gi\", cpu: \"500m\" }, requests: { cpu: \"lots\" } },\n  }]\n}\n";
        let want = "resource k8s.deployment d {\n  metadata.name = \"2Gi\"\n  \
                    spec.template.spec.containers = [{\n    name: \"a\",\n    \
                    resources: { limits: { memory: 2Gi, cpu: 500m }, requests: { cpu: \"lots\" } },\n  }]\n}\n";
        assert_eq!(fmt(src), want);
    }

    /// Through a variable bound over a keyed list's elements (R-69).
    #[test]
    fn a_set_through_an_element_is_typed() {
        let src = "set c.resources.limits = { memory: \"1Gi\" } @default where {\n  \
                   w in k8s.deployment\n  c in w.spec.template.spec.containers\n}\n";
        let want = "set c.resources.limits = { memory: 1Gi } @default where {\n  \
                    w in k8s.deployment\n  c in w.spec.template.spec.containers\n}\n";
        assert_eq!(fmt(src), want);
    }

    /// An input's default, an instance's entry for its component's input,
    /// a function's typed parameter: a constructor of a string that is
    /// the value as written is dropped.
    #[test]
    fn a_redundant_constructor_is_dropped() {
        let src = "input size: bytes = \"512Mi\"\ninput ttl: duration = \"30d\"\n\
                   input net: inet = inet(\"10.0.0.0/16\")\ninput host: inet = inet(\"10.0.0.1/16\")\n\n\
                   component c {\n  input cidr: inet\n}\n\
                   instance c a { cidr = inet(\"10.1.0.0/16\") }\n\
                   let s = inet.subnet(inet(\"10.0.0.0/16\"), 8, 1)\n\
                   let t = inet.subnet(inet(net), 8, 1)\n";
        let want = "input size: bytes = 512Mi\ninput ttl: duration = 30d\n\
                    input net: inet = \"10.0.0.0/16\"\ninput host: inet = inet(\"10.0.0.1/16\")\n\n\
                    component c {\n  input cidr: inet\n}\n\
                    instance c a { cidr = \"10.1.0.0/16\" }\n\
                    let s = inet.subnet(\"10.0.0.0/16\", 8, 1)\n\
                    let t = inet.subnet(inet(net), 8, 1)\n";
        assert_eq!(fmt(src), want);
    }
}
