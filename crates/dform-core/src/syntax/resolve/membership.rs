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

/// A membership being lowered, `x in e` at `span`: its sides as written,
/// the chain its right side is, and whether it is `x in resource`.
struct Member {
    n: SyntaxNode,
    span: Span,
    lhs: SyntaxNode,
    rhs: Option<SyntaxNode>,
    chain: Option<Chain>,
    any_type: bool,
}

impl Member {
    /// `x in world.T`: the chain of the world's type.
    fn world(&self) -> Option<&Chain> {
        self.chain
            .as_ref()
            .filter(|c| c.head == "world" && !c.ops.is_empty())
    }
}

/// A membership's literal, and the goal it is built as.
type Membership = L<(Lit, Option<GoalId>)>;

/// The type `x in T` ranges `x` over, and for a provider's type a `use ..
/// as` also names (R-115), the test of the type's names and its
/// collection.
type Typed = (Option<Term>, Option<(Lit, Coll)>);

impl Lowerer<'_> {
    /// `x in T` (a generator over `want`), `x in resource`, `x in world.T`,
    /// or `x in list`.
    pub(super) fn membership(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
    ) -> Membership {
        let span = self.span(n);
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let lhs = ts.first().ok_or(Skip)?.clone();
        let any_type = tokens(n).any(|t| t.kind() == RESOURCE_KW);
        let chain = ts.get(1).and_then(Chain::of).map(|c| self.type_each(rc, c));
        let m = Member {
            n: n.clone(),
            span,
            lhs,
            rhs: ts.get(1).cloned(),
            chain,
            any_type,
        };
        // `x in f`, `f` a let with parameters (R-187): nothing enumerates it.
        if let Some(c) = m
            .chain
            .as_ref()
            .filter(|c| c.is_bare() && !rc.vars.contains_key(&c.head))
            && let Some((_, def)) = self.function_def(rc.scope, &c.head)
        {
            let at = self.span_of(c.range);
            return self
                .in_function(at, &c.head.clone(), &def)
                .map(|l| (l, None));
        }
        if let (Some(c), Some(rhs)) = (&m.chain, &m.rhs)
            && c.ops.iter().any(each::is_each)
        {
            return self.each_member(rc, &m, rhs, out);
        }
        if !m.any_type
            && let Some(named) = self.named_member(rc, &m, out)?
        {
            return Ok(named);
        }
        let (typ, renamed) = self.member_type(rc, &m);
        if typ.is_some() || m.world().is_some() {
            return self.typed_member(rc, &m, (typ, renamed), out);
        }
        self.list_member(rc, &m, out)
    }

    /// `v in PATH` with a `[_]` in it binds `v` to each value the path
    /// reaches (R-162): the steps enumerate.
    fn each_member(
        &mut self,
        rc: &mut Rc,
        m: &Member,
        rhs: &SyntaxNode,
        out: &mut Vec<Lit>,
    ) -> Membership {
        if m.lhs.kind() == TUPLE {
            return self.error(
                m.span,
                format!(
                    "`{}` binds each value of a path with `[_]`, one name: `v in {}`",
                    m.lhs.text(),
                    rhs.text()
                ),
            );
        }
        let (value, v) = self.bind(false, |l| l.arg_node(rc, rhs, Pos::Content, out))?;
        let (item, i) = self.arg_node(rc, &m.lhs, Pos::Content, out)?;
        let g = self.member_goal(rc, m.span, i, Coll::Each(v));
        Ok((Lit::Eq(item, value), Some(g)))
    }

    /// `x in T` of a named type: an enum type's values (R-70), a
    /// provider's namespace's resources (R-49), a component's copies
    /// (R-67); none for any other.
    fn named_member(
        &mut self,
        rc: &mut Rc,
        m: &Member,
        out: &mut Vec<Lit>,
    ) -> L<Option<(Lit, Option<GoalId>)>> {
        let (Some(c), Some(rhs)) = (&m.chain, &m.rhs) else {
            return Ok(None);
        };
        // `x in T`, `T` an enum type (R-70): each of its values, in
        // order, as a range is enumerated.
        if let Some(e) = self.enum_type(rc, c, rhs)? {
            return self.enum_member(rc, &m.lhs, e, out, m.span).map(Some);
        }
        // `r in NS`, a provider's namespace (R-49): a resource of any of
        // its types.
        if let Some(ns) = self.namespace_of(rc, c) {
            return self
                .namespace_member(rc, &m.lhs, &ns, out, m.span)
                .map(Some);
        }
        match self.component_of(rc, c) {
            Some(path) => self.copies_member(rc, m, path, out).map(Some),
            None => Ok(None),
        }
    }

    /// `x in network`, a component (R-67): each of its copies; `blue in
    /// network`, a copy in scope by its name (R-113), is the test that it
    /// is one of them.
    fn copies_member(
        &mut self,
        rc: &mut Rc,
        m: &Member,
        path: String,
        out: &mut Vec<Lit>,
    ) -> Membership {
        let span = m.span;
        let copy = Chain::of(&m.lhs)
            .filter(|l| l.is_bare() && !rc.vars.contains_key(&l.head))
            .filter(|l| self.instance_in(rc.scope, &l.head).is_some())
            .map(|l| str_term(&l.head));
        let (x, xid) = match copy {
            Some(name) => {
                let id = self.name_node(rc, span, &name);
                (name, id)
            }
            None => self.arg_node(rc, &m.lhs, Pos::Content, out)?,
        };
        let parent = self.copies_scope(rc, &path, span);
        let lit = Lit::Pos(atom_at(
            crate::modules::INSTANCE_OF,
            vec![str_term(&path), self.term_of(parent), x],
            span,
        ));
        let coll = Coll::Copies {
            component: path,
            scope: parent,
        };
        let g = self.member_goal(rc, span, xid, coll);
        Ok((lit, Some(g)))
    }

    /// The type `x in T` ranges over: `T`, a variable for `x in resource`;
    /// for `x in ovh.instance` with the provider also used as `ca`
    /// (R-115), a resource of `ovh.instance` or `ca.instance`,
    /// `__provider_type("ovh.instance", Type), want(Type, x)`.
    fn member_type(&mut self, rc: &mut Rc, m: &Member) -> Typed {
        let span = m.span;
        let mut typ = if m.any_type {
            let lhs = Chain::of(&m.lhs).map(|c| c.head).unwrap_or_default();
            Some(match rc.types.get(&lhs) {
                Some(t) => t.clone(),
                None => var(&fresh(rc, "Type")),
            })
        } else {
            m.chain
                .as_ref()
                .and_then(|c| self.chain_type(rc, c))
                .map(|t| str_term(&t))
        };
        let mut renamed = None;
        if let Some(Term::Val(Value::Str(t))) = typ.clone()
            && m.world().is_none()
            && self.covering(&t).len() > 1
        {
            let lhs = Chain::of(&m.lhs).filter(Chain::is_bare).map(|c| c.head);
            let tv = match lhs.as_ref().and_then(|h| rc.types.get(h)) {
                Some(v @ Term::Var(_)) => v.clone(),
                _ => var(&fresh(rc, "Type")),
            };
            for each in self.covering(&t) {
                self.helpers.push(Stmt::Fact(atom_at(
                    PROVIDER_TYPE,
                    vec![str_term(&t), str_term(&each)],
                    span,
                )));
            }
            let test = Lit::Pos(atom_at(PROVIDER_TYPE, vec![str_term(&t), tv.clone()], span));
            let coll = Coll::ProviderType {
                typ: t.clone(),
                names: self.covering(&t),
                var: self.name_node(rc, span, &tv),
                enumerate: false,
            };
            renamed = Some((test, coll));
            typ = Some(tv);
        }
        (typ, renamed)
    }

    /// `x in T`, `x in resource`, `x in world.T`: the element, bound or
    /// checked (a bare resource name is its address, a computed name is
    /// bound first), then the type's test.
    fn typed_member(
        &mut self,
        rc: &mut Rc,
        m: &Member,
        (typ, renamed): Typed,
        out: &mut Vec<Lit>,
    ) -> Membership {
        let span = m.span;
        if m.lhs.kind() == TUPLE {
            return self.error(
                span,
                format!(
                    "`{}` is a pattern: a resource is enumerated by a name, `r in {}`",
                    m.lhs.text(),
                    m.rhs
                        .as_ref()
                        .map(|t| t.text().to_string())
                        .unwrap_or_default()
                ),
            );
        }
        let lhs_id = self.member_element(rc, m, &typ, out)?;
        let lhs = self.term_of(lhs_id);
        // `r in T` with `r` a reference column's (`deformation(k, r, _)`):
        // the column's `ref(T, R, "")` tests the type, so a row of a
        // deleted resource passes (R-42).
        if let (Some(t), Some(c)) = (&typ, Chain::of(&m.lhs))
            && m.world().is_none()
            && c.is_bare()
            && Self::ref_bound(&m.n, &c.head)
        {
            if let Some((test, coll)) = renamed {
                // A test alone: its element as written.
                let coll = Self::tested(coll, false);
                let g = self.member_goal(rc, span, lhs_id, coll);
                return Ok((test, Some(g)));
            }
            let have = rc.types.get(&c.head).cloned().unwrap_or_else(|| t.clone());
            let (h, ty) = (self.name_node(rc, span, &have), self.name_node(rc, span, t));
            let coll = Coll::TypeOf { ty, of: lhs_id };
            let g = self.member_goal(rc, span, h, coll);
            return Ok((Lit::Eq(have, t.clone()), Some(g)));
        }
        if let Some(w) = m.world() {
            let typ = w.fields()[1..].join(".");
            let lit = Lit::Pos(atom_at("cloud_exists", vec![str_term(&typ), lhs], span));
            let g = self.member_goal(rc, span, lhs_id, Coll::World(typ));
            return Ok((lit, Some(g)));
        }
        self.address_key(&lhs, span)?;
        let typ = typ.expect("a type or the world");
        let coll = match renamed {
            Some((test, coll)) => {
                out.push(test);
                Self::tested(coll, true)
            }
            None => Coll::Type(self.name_node(rc, span, &typ)),
        };
        let g = self.member_goal(rc, span, lhs_id, coll);
        Ok((Lit::Pos(atom_at("want", vec![typ, lhs], span)), Some(g)))
    }

    /// The element of `x in T`: a bare resource name its address (the
    /// type on the right picks among resources of one name), a computed
    /// name bound first, else the term.
    fn member_element(
        &mut self,
        rc: &mut Rc,
        m: &Member,
        typ: &Option<Term>,
        out: &mut Vec<Lit>,
    ) -> L<ExprId> {
        let span = m.span;
        Ok(match Chain::of(&m.lhs) {
            Some(c) if c.is_bare() && self.resource(rc.scope, &c.head).is_some() => {
                let named = self.resource(rc.scope, &c.head).unwrap_or_default();
                match typ {
                    Some(Term::Val(Value::Str(t))) if named.contains(t) => {
                        self.resource_addr(rc.scope, &c.head, span)
                    }
                    _ => self.reference(rc, &c, out, span)?.1,
                }
            }
            _ => {
                let (t, id) = self.arg_node(rc, &m.lhs, Pos::Content, out)?;
                if matches!(t, Term::Func { .. }) {
                    let v = fresh(rc, "Name");
                    out.push(Lit::Eq(var(&v), t));
                    self.building(rc, span, |b| b.bound_into(id, &v))
                } else {
                    id
                }
            }
        })
    }

    /// `x in list`, `(k, v) in o`: `member`.
    fn list_member(&mut self, rc: &mut Rc, m: &Member, out: &mut Vec<Lit>) -> Membership {
        let (span, lhs) = (m.span, &m.lhs);
        let rhs = m.rhs.as_ref().ok_or(Skip)?;
        let of = self.resource_list(rc, rhs);
        let (list, list_id) = self.bind(false, |l| l.arg_node(rc, rhs, Pos::Content, out))?;
        // The element's name, for `set c.p` (R-69): `c in L`, `(i, c) in L`.
        let elem = match lhs.kind() {
            TUPLE => terms(lhs).nth(1),
            _ => Some(lhs.clone()),
        };
        if let (Some(of), Some(c)) = (of, elem.as_ref().and_then(Chain::of))
            && c.is_bare()
        {
            rc.elems.insert(c.head, of);
        }
        if lhs.kind() == TUPLE {
            return self.pattern_in(rc, lhs, (list, Coll::Expr(list_id)), out, span);
        }
        let (item, i) = self.arg_node(rc, lhs, Pos::Content, out)?;
        let g = self.element_goal(rc, span, i, list_id);
        Ok((Lit::Pos(atom_at("member", vec![list, item], span)), Some(g)))
    }

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
    ) -> L<(Lit, Option<GoalId>)> {
        let values = members.iter().cloned().map(Value::Str).collect();
        self.helpers.push(Stmt::Fact(atom_at(
            ENUM,
            vec![str_term(&name), Term::Val(Value::List(values))],
            def,
        )));
        let v = fresh(rc, "Values");
        let list = var(&v);
        let of = self.str_node(span, &name);
        let read = self.building(rc, span, |b| b.call_into((ENUM, Reach::Own), vec![of], &v));
        self.hoisted_into(read, out);
        let coll = Coll::Enum {
            name,
            values: members,
            def,
            list: read,
        };
        if lhs.kind() == TUPLE {
            return self.pattern_in(rc, lhs, (list, coll), out, span);
        }
        let (item, i) = self.arg_node(rc, lhs, Pos::Content, out)?;
        let g = self.building(rc, span, |b| {
            let pat = b.element_of(i);
            b.goal_node(span, GoalKind::Member { pat, coll })
        });
        Ok((Lit::Pos(atom_at("member", vec![list, item], span)), Some(g)))
    }

    /// The declaration of the input `name` in scope.
    fn input_node(&self, scope: ScopeId, name: &str) -> Option<SyntaxNode> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).input(name).cloned())
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
            || self.program.scopes.declares_relation(h)
            || self.chain_of(rc.scope).into_iter().any(|s| {
                let s = self.names(s);
                s.module(h).is_some() || s.instance(h).is_some() || s.component(h).is_some()
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
    ) -> L<(Lit, Option<GoalId>)> {
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
        let addr = self.var_named(rc, &c.head, span);
        let types = self.namespace_types(ns);
        for t in &types {
            let mut fact = atom_at(NAMESPACE, vec![str_term(ns), str_term(t)], span);
            fact.span = span;
            self.helpers.push(Stmt::Fact(fact));
        }
        let test = Lit::Pos(atom_at(NAMESPACE, vec![str_term(ns), typ.clone()], span));
        let coll = Coll::Namespace {
            ns: ns.to_string(),
            types,
            typ: self.name_node(rc, span, &typ),
            enumerate: !bound,
        };
        let x = match bound {
            true => self.node(span, ExprKind::Hole),
            false => self.var_node(rc, span, &addr),
        };
        let g = self.member_goal(rc, span, x, coll);
        if bound {
            return Ok((test, Some(g)));
        }
        out.push(test);
        Ok((
            Lit::Pos(atom_at("want", vec![typ, var(&addr)], span)),
            Some(g),
        ))
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
