//! Membership over a named type's values and over a provider's namespace
//! (docs/grammar.md "Membership").
//!
//! - `x in T`, `T` an enum type alias (`type environment = enum("staging",
//!   "prod")`), binds `x` to each value in declaration order (R-70),
//!   lowered as a range is, over the fact `__enum("environment",
//!   ["staging", "prod"])` the type's declaration states: `__enum(T, L),
//!   member(L, X)`, so `why` shows the type as the leaf. `x in env`,
//!   `env` an input of an enum type, is a cell, not a type: an error that
//!   says to name the type.
//! - `r in NS`, `NS` a provider's namespace (`k8s`, R-36), is a resource
//!   of any type in it (R-49): `__namespace("k8s", Type), want(Type, R)`,
//!   the facts `__namespace("k8s", T)` one per type of the namespace the
//!   program knows (its own types, and the built-in schemas' the provider
//!   of that name serves). With `r` already bound, by a reference column
//!   (`deformation(k, r, _)`), it is a type test. `r.p` reads an attribute
//!   every type of the namespace the compiler knows the attributes of has;
//!   otherwise an error names the types that lack it.
//! - `r in T`, `T` a resource type, with `r` a reference column's
//!   anywhere in the body (`deformation(k, r, _)`, written before or
//!   after), is a type test as well, so a plan row of a deleted resource
//!   binds: the column takes the reference apart as `ref("T", R, "")`,
//!   which is the test. `r not in T` there (or `not r in T`) takes it
//!   apart as `ref(Type, R, "")` and tests `Type != "T"` (R-219).
//! - `r in T`, `T` a provider's type (`ovh.instance`) whose provider a
//!   `use ovh as ca` also names (R-115), is a resource of `T` or of the
//!   same type under each such name: `__provider_type("ovh.instance",
//!   Type), want(Type, R)`, one fact per name.

use super::*;

/// The relation of the enum types `x in T` ranges over: `__enum(T,
/// [values])`, one fact at the type's declaration, so `why` names it.
pub(super) const ENUM: &str = "__enum";

/// The relation of a namespace's types: `__namespace(NS, T)`.
pub(super) const NAMESPACE: &str = "__namespace";

/// The types `x in T` ranges over, `T` a provider's type that a `use ..
/// as` also names otherwise (R-115): `__provider_type("ovh.instance",
/// "ca.instance")`, and `T` itself.
pub(super) const PROVIDER_TYPE: &str = "__provider_type";

/// What the built-in schemas say of a type: its attribute paths, and the
/// providers that serve it (`type_provider`).
#[derive(Default)]
struct Known {
    paths: BTreeSet<String>,
    providers: BTreeSet<String>,
}

/// The built-in schemas' types, read from their text (`type_*` rows).
fn builtin_types() -> &'static BTreeMap<String, Known> {
    static TYPES: std::sync::OnceLock<BTreeMap<String, Known>> = std::sync::OnceLock::new();
    TYPES.get_or_init(|| {
        let mut out: BTreeMap<String, Known> = BTreeMap::new();
        for name in ["fake", "gke", "k8s", "aws-mock"] {
            let Some(src) = crate::schema::builtin(name) else {
                continue;
            };
            for line in src.lines() {
                let Some((head, rest)) = line.trim().split_once('(') else {
                    continue;
                };
                if !head.starts_with("type_") {
                    continue;
                }
                let mut args = rest.split(',').map(|a| a.trim().trim_matches(['"', ')']));
                let Some(t) = args.next().filter(|t| !t.is_empty()) else {
                    continue;
                };
                let known = out.entry(t.to_string()).or_default();
                match (head, args.next()) {
                    ("type_attr", Some(p)) => {
                        known.paths.insert(p.to_string());
                    }
                    ("type_provider", Some(p)) => {
                        known.providers.insert(p.to_string());
                    }
                    _ => {}
                }
            }
        }
        out
    })
}

impl Lowerer<'_> {
    /// The enum type a membership's right side names, its values; `None`
    /// when it names no type alias. An input of an enum type is an error:
    /// a cell holds one value.
    pub(super) fn enum_type(
        &mut self,
        rc: &Rc,
        c: &Chain,
        at: &SyntaxNode,
    ) -> L<Option<(String, Vec<String>, Span)>> {
        if !c.ops.iter().all(|o| matches!(o, Op::Field(..))) {
            return Ok(None);
        }
        let head = &c.head;
        if rc.vars.contains_key(head)
            || rc.types.contains_key(head)
            || self.resource(rc.scope, head).is_some()
            || self.find_let(rc.scope, head).is_some()
        {
            return Ok(None);
        }
        if self.is_value(rc.scope, head) {
            if !c.is_bare() {
                return Ok(None);
            }
            let Some(t) = self
                .input_node(rc.scope, head)
                .and_then(|n| node(&n, TYPE_EXPR))
            else {
                return Ok(None);
            };
            let first = tokens(&t).next().map(|t| t.text().to_string());
            let named = match first.as_deref() {
                Some("enum") | None => None,
                Some(_) => Some(t.text().to_string().trim().to_string()),
            };
            let members = match &named {
                Some(n) => self.alias(&t, n).and_then(|x| crate::types::members(&x)),
                None if first.as_deref() == Some("enum") => Some(Vec::new()),
                None => None,
            };
            if members.is_none() {
                return Ok(None);
            }
            let span = self.span(at);
            let ty = t.text().to_string().trim().to_string();
            let help = match named {
                Some(n) => format!("range over its type: `x in {n}`"),
                None => format!(
                    "an inline `enum(..)` has no name: name it, `type T = {ty}`, declare \
                     `input {head}: T`, and range over it, `x in T`"
                ),
            };
            let d = Diagnostic::error(
                span,
                format!(
                    "`{head}` is an input, one value of {ty}: `x in T` ranges over a type's \
                     values, so name the type"
                ),
            )
            .with_help(help);
            self.diags.push(d);
            return Err(Skip);
        }
        let name = c.fields().join(".");
        let Some((ty, def)) = self.alias_def(at, &name) else {
            return Ok(None);
        };
        match crate::types::members(&ty) {
            Some(ms) => Ok(Some((name, ms, def))),
            None => self.error(
                self.span(at),
                format!(
                    "`{name}` is the type {}: `x in T` ranges over an enum type's values, a \
                     resource type's resources or a provider's namespace",
                    crate::inputs::type_text(&ty)
                ),
            ),
        }
    }

    /// `x in T`, `T` the enum type `name` declared at `def`: its values
    /// in order, read from the type's fact.
    pub(super) fn enum_member(
        &mut self,
        rc: &mut Rc,
        lhs: &SyntaxNode,
        (name, members, def): (String, Vec<String>, Span),
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<Lit> {
        let values = Term::Val(Value::List(members.into_iter().map(Value::Str).collect()));
        self.helpers.push(Stmt::Fact(atom_at(
            ENUM,
            vec![str_term(&name), values],
            def,
        )));
        let list = var(&fresh(rc, "Values"));
        out.push(Lit::Pos(atom_at(
            ENUM,
            vec![str_term(&name), list.clone()],
            span,
        )));
        if lhs.kind() == TUPLE {
            return self.pattern_in(rc, lhs, list, out, span);
        }
        let item = self.term(rc, lhs, Pos::Content, out)?;
        Ok(Lit::Pos(atom_at("member", vec![list, item], span)))
    }

    /// The declaration of the input `name` in scope.
    fn input_node(&self, scope: ScopeId, name: &str) -> Option<SyntaxNode> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.decls.scopes[s].input_nodes.get(name).cloned())
    }

    /// The provider namespace a bare name is, where nothing else of that
    /// name is in scope: `k8s`.
    pub(super) fn namespace_of(&self, rc: &Rc, c: &Chain) -> Option<String> {
        let h = &c.head;
        let shadowed = rc.vars.contains_key(h)
            || rc.types.contains_key(h)
            || self.is_value(rc.scope, h)
            || self.resource(rc.scope, h).is_some()
            || self.module_at(h).is_some()
            || self.decls.relations.contains(h)
            || self.chain_of(rc.scope).into_iter().any(|s| {
                let s = &self.decls.scopes[s];
                s.uses.contains_key(h)
                    || s.instances.contains_key(h)
                    || s.components.contains_key(h)
            });
        (c.is_bare() && !shadowed && self.decls.namespaces.contains(h)).then(|| h.clone())
    }

    /// The types of the namespace `ns` the program knows: its own, and
    /// the built-in schemas' that the provider `ns` serves (R-36: a
    /// provider's name is its namespace; the fake mock's `k8s.cluster` is
    /// `fakecloud`'s).
    fn namespace_types(&self, ns: &str) -> Vec<String> {
        let prefix = format!("{ns}.");
        let known = builtin_types();
        let served = |t: &str| match known.get(t) {
            Some(k) if !k.providers.is_empty() => k.providers.contains(ns),
            _ => true,
        };
        self.decls
            .types
            .iter()
            .filter(|t| t.starts_with(&prefix) && served(t))
            .cloned()
            .collect()
    }

    /// Is `name` the reference column of a plan row or lifecycle relation
    /// (`deformation(k, name, _)`) the body of `n`'s statement reads, in
    /// any order (R-10)? `name in T` is then a type test.
    pub(super) fn ref_bound(n: &SyntaxNode, name: &str) -> bool {
        let lit = |k: SyntaxKind| {
            matches!(
                k,
                LIT_ATOM | LIT_TRUTH | LIT_HAS | LIT_CMP | LIT_IN | LIT_NOT_IN | LIT_NOT
            )
        };
        let Some(stmt) = n
            .ancestors()
            .find(|a| !matches!(a.kind(), BODY | CLAUSE) && !lit(a.kind()))
        else {
            return false;
        };
        let negated = |a: &SyntaxNode| {
            a.ancestors()
                .take_while(|x| x != &stmt)
                .any(|x| matches!(x.kind(), LIT_NOT | LIT_NOT_BLOCK | COMPREHENSION))
        };
        stmt.descendants()
            .filter(|a| a.kind() == LIT_ATOM && !negated(a))
            .filter_map(|a| terms(&a).next())
            .any(|call| {
                let Some(pred) = call.children().find_map(|c| Chain::of(&c)) else {
                    return false;
                };
                let args: Vec<SyntaxNode> = node(&call, ARG_LIST)
                    .map(|l| terms(&l).collect())
                    .unwrap_or_default();
                crate::zset::ref_column(&pred.fields().join("."), args.len())
                    .and_then(|i| args.get(i).and_then(Chain::of))
                    .is_some_and(|c| c.is_bare() && c.head == name)
            })
    }

    /// `r in NS`: the facts of the namespace's types, and the literal.
    pub(super) fn namespace_member(
        &mut self,
        rc: &mut Rc,
        lhs: &SyntaxNode,
        ns: &str,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<Lit> {
        let Some(c) = Chain::of(lhs).filter(Chain::is_bare) else {
            return self.error(
                span,
                format!("a namespace's resources are enumerated by a name: `r in {ns}`"),
            );
        };
        let bound = rc.vars.contains_key(&c.head) || Self::ref_bound(lhs, &c.head);
        let typ = match rc.types.get(&c.head) {
            Some(t) => t.clone(),
            None => var(&fresh(rc, "Type")),
        };
        let addr = var(&self.var_named(rc, &c.head, span));
        for t in self.namespace_types(ns) {
            let mut fact = atom_at(NAMESPACE, vec![str_term(ns), str_term(&t)], span);
            fact.span = span;
            self.helpers.push(Stmt::Fact(fact));
        }
        let test = Lit::Pos(atom_at(NAMESPACE, vec![str_term(ns), typ.clone()], span));
        if bound {
            return Ok(test);
        }
        out.push(test);
        Ok(Lit::Pos(atom_at("want", vec![typ, addr], span)))
    }

    /// `r.p` on a variable a namespace ranges: an error naming the types
    /// of the namespace that have no `p`.
    pub(super) fn namespace_attr(&mut self, rc: &Rc, h: &str, first: &str, span: Span) -> L<()> {
        let Some(ns) = rc.namespaces.get(h).cloned() else {
            return Ok(());
        };
        let known = builtin_types();
        let lacking: Vec<String> = self
            .namespace_types(&ns)
            .into_iter()
            .filter(|t| {
                known.get(t).is_some_and(|k| {
                    !k.paths.is_empty()
                        && !k
                            .paths
                            .iter()
                            .any(|p| p == first || p.starts_with(&format!("{first}.")))
                })
            })
            .collect();
        if lacking.is_empty() {
            return Ok(());
        }
        self.error(
            span,
            format!(
                "`{h}.{first}`: {h} is a resource of any {ns} type, and {} {} no `{first}`",
                lacking.join(", "),
                if lacking.len() == 1 { "has" } else { "have" }
            ),
        )
    }
}
