//! A program relation's columns that hold references (R-185): a rule
//! whose head writes a variable `x in T` binds, `x in resource` or another
//! such column binds, or a resource by its name, gives its column the
//! reference, so `workload(w) where w in k8s.deployment` holds
//! `ref(k8s.deployment, "web", "")`, never the address alone, and a body
//! that reads `workload(w)` has `w` as that resource: `w.spec` reads its
//! attribute, `set w.spec.x = v where workload(w)` writes it. A relation
//! of several types (a rule per type) holds each row's own, read by the
//! row's type.
//!
//! The plan's relations (`deformation`, `requires_approval`, ..) have
//! their reference column fixed (`zset::ref_column`); a program's are found
//! here, before any statement is lowered, to a fixpoint over the rules
//! (a column a reference column's variable fills is one too).

use super::*;

/// What a program relation's reference column holds: the types its rows
/// give it, or any resource's.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct RefColumn {
    types: BTreeSet<String>,
    any: bool,
    /// The name the first rule's head writes in it (`w`), for a message.
    name: Option<String>,
}

impl RefColumn {
    /// The one type its rows have, when they have one.
    fn typ(&self) -> Option<&String> {
        match (self.any, self.types.len()) {
            (false, 1) => self.types.first(),
            _ => None,
        }
    }

    /// The column's type as a message prints it: `ref(k8s.deployment |
    /// k8s.stateful_set)`, `a reference` when any resource's.
    fn shown(&self) -> String {
        match self.any {
            true => "a reference".to_string(),
            false => format!(
                "ref({})",
                self.types.iter().cloned().collect::<Vec<_>>().join(" | ")
            ),
        }
    }

    fn merge(&mut self, other: RefColumn) -> bool {
        let before = self.clone();
        self.types.extend(other.types);
        self.any |= other.any;
        if self.name.is_none() {
            self.name = other.name;
        }
        *self != before
    }
}

/// A relation by the scope it is declared in and its name there.
pub(super) type RelKey = (usize, String);

impl Lowerer<'_> {
    /// The relation `pred` (as an atom writes it, `p` or `m::p`) read or
    /// written in `scope`: the nearest scope whose rules define it, or the
    /// module `m` a `use` binds.
    pub(super) fn relation_key(&self, scope: usize, pred: &str) -> Option<RelKey> {
        if let Some((m, p)) = pred.split_once("::") {
            let path = self.use_in(scope, m)?;
            let s = self.decls.modules.get(&path)?.scope;
            return Some((s, p.to_string()));
        }
        self.chain_of(scope)
            .into_iter()
            .map(|s| self.decl_scope(s))
            .find(|s| self.decls.scopes[*s].heads.contains(pred))
            .map(|s| (s, pred.to_string()))
    }

    /// The reference columns of the relation an atom's callee `call`
    /// names in `scope`, with `arity` arguments.
    fn call_ref_columns(
        &self,
        scope: usize,
        call: &SyntaxNode,
        arity: usize,
    ) -> BTreeMap<usize, RefColumn> {
        match self.callee(call) {
            Some(pred) => self.ref_columns(scope, &self.written_relation(scope, pred), arity),
            None => BTreeMap::new(),
        }
    }

    /// The reference columns of the relation `pred` of arity `arity` in
    /// `scope`, by index.
    pub(super) fn ref_columns(
        &self,
        scope: usize,
        pred: &str,
        arity: usize,
    ) -> BTreeMap<usize, RefColumn> {
        self.relation_key(scope, pred)
            .and_then(|k| self.decls.refs.get(&(k, arity)))
            .cloned()
            .unwrap_or_default()
    }

    /// The type a variable a reference column binds takes: the column's
    /// one type, else the row's, a fresh type variable.
    pub(super) fn ref_column_type(rc: &mut Rc, col: &RefColumn) -> Term {
        match col.typ() {
            Some(t) => str_term(t),
            None => var(&fresh(rc, "Type")),
        }
    }

    /// Every program relation's reference columns, to a fixpoint over the
    /// rules `collect` found.
    pub(super) fn find_ref_columns(&mut self) {
        let rules = self.decls.rules.clone();
        loop {
            let mut changed = false;
            for (scope, n) in &rules {
                for (rel, i, col) in self.head_refs(*scope, n) {
                    let cols = self.decls.refs.entry(rel).or_default();
                    changed |= cols.entry(i).or_default().merge(col);
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// The reference columns the head of the rule or fact `n` in `scope`
    /// writes: each argument that is a variable the statement types as a
    /// resource, or a resource's name. By relation and arity, and index.
    fn head_refs(&self, scope: usize, n: &SyntaxNode) -> Vec<((RelKey, usize), usize, RefColumn)> {
        let Some(head) = n.children().find(|c| c.kind() == CALL) else {
            return Vec::new();
        };
        let Some(key) = self
            .callee(&head)
            .and_then(|p| self.relation_key(scope, &p))
        else {
            return Vec::new();
        };
        // A relation a copy exports or its user gives crosses the module's
        // boundary, where `modules` scopes its rows as addresses: it holds
        // them as it did.
        let boundary = &self.decls.scopes[key.0];
        if boundary.relation_outputs.contains(&key.1) || boundary.relation_inputs.contains(&key.1) {
            return Vec::new();
        }
        let args: Vec<SyntaxNode> = node(&head, ARG_LIST)
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let rc = self.rc(n, scope, &Rc::default());
        let mut out = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let Some(c) = Chain::of(a).filter(Chain::is_bare) else {
                continue;
            };
            let mut col = RefColumn {
                name: Some(c.head.clone()),
                ..RefColumn::default()
            };
            match rc.types.get(&c.head) {
                Some(Term::Val(Value::Str(t))) => {
                    col.types.insert(t.clone());
                }
                Some(_) => col.any = true,
                None if !rc.vars.contains_key(&c.head) => match self.resource(scope, &c.head) {
                    Some(types) => col.types.extend(types),
                    None => continue,
                },
                None => continue,
            }
            out.push(((key.clone(), args.len()), i, col));
        }
        out
    }

    /// `p(x)` positive in the statement `n`, `p`'s column a reference: `x`
    /// is that resource wherever the statement uses it (its type the
    /// column's, or the row's), unless `x in T` typed it already.
    pub(super) fn ref_column_vars(&self, n: &SyntaxNode, scope: usize, rc: &mut Rc) {
        if self.decls.refs.is_empty() {
            return;
        }
        let negated = |a: &SyntaxNode| {
            a.ancestors()
                .take_while(|x| x != n)
                .any(|x| matches!(x.kind(), LIT_NOT | LIT_NOT_BLOCK))
        };
        for a in n.descendants().filter(|a| a.kind() == LIT_ATOM) {
            if negated(&a) {
                continue;
            }
            let Some(call) = terms(&a).next() else {
                continue;
            };
            let args: Vec<SyntaxNode> = node(&call, ARG_LIST)
                .map(|l| terms(&l).collect())
                .unwrap_or_default();
            for (i, col) in self.call_ref_columns(scope, &call, args.len()) {
                let Some(c) = args.get(i).and_then(Chain::of).filter(Chain::is_bare) else {
                    continue;
                };
                if c.head == "_"
                    || rc.types.contains_key(&c.head)
                    || rc.vars.contains_key(&c.head)
                    || self.resource(scope, &c.head).is_some()
                {
                    continue;
                }
                let typ = Self::ref_column_type(rc, &col);
                rc.types.insert(c.head, typ);
            }
        }
    }

    /// A relation's name as an atom lowers it: `m.p`, `m` a module a `use`
    /// binds, is `m::p`.
    fn written_relation(&self, scope: usize, pred: String) -> String {
        match pred.split_once('.') {
            Some((m, p)) if self.use_in(scope, m).is_some() => format!("{m}::{p}"),
            _ => pred,
        }
    }

    /// The arguments `list` of an atom of `pred`, a program relation with
    /// reference columns: a resource there is the reference (`ref_term`),
    /// and a literal is an error naming the column and its type.
    pub(super) fn ref_args(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        list: Option<&SyntaxNode>,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Vec<Term>> {
        let list: Vec<SyntaxNode> = list.map(|l| terms(l).collect()).unwrap_or_default();
        let cols = self.ref_columns(rc.scope, pred, list.len());
        let mut args = Vec::new();
        for (i, t) in list.iter().enumerate() {
            args.push(match cols.get(&i) {
                Some(col) if t.kind() == LITERAL => {
                    return self.literal_in_ref_column(pred, i, col, t);
                }
                Some(col) if Self::typed(rc, t) => {
                    let r = self.ref_term(rc, t, pos, pre)?;
                    if pos == Pos::Content {
                        Self::row_types(rc, t, col, pre);
                    }
                    r
                }
                _ if pos == Pos::Content && t.kind() == TUPLE => self.pattern(rc, t, pre)?,
                _ => self.term(rc, t, pos, pre)?,
            });
        }
        Ok(args)
    }

    /// Whether `t` is a resource where a reference column takes it: a
    /// variable `in` or a reference column typed, not a free one.
    fn typed(rc: &Rc, t: &SyntaxNode) -> bool {
        Chain::of(t)
            .filter(Chain::is_bare)
            .is_none_or(|c| rc.types.contains_key(&c.head) || rc.vars.contains_key(&c.head))
    }

    /// A row's type where the column holds several: one of them,
    /// `member([T1, T2], Type)` before the atom, so what reads by it (a
    /// `set` through the column, `types::read`) knows each it may be.
    fn row_types(rc: &Rc, t: &SyntaxNode, col: &RefColumn, pre: &mut Vec<Lit>) {
        let Some(c) = Chain::of(t).filter(Chain::is_bare) else {
            return;
        };
        let Some(tv @ Term::Var(_)) = rc.types.get(&c.head) else {
            return;
        };
        if col.any || col.types.len() < 2 {
            return;
        }
        let types = col.types.iter().map(|t| str_term(t)).collect();
        pre.push(Lit::Pos(Atom {
            pred: "member".into(),
            args: vec![Term::List(types), tv.clone()],
            record: None,
            span: Span::default(),
        }));
    }

    /// A literal where a program relation's column holds references: the
    /// error at it, naming the column and its type, and the resource to
    /// write.
    fn literal_in_ref_column<T>(
        &mut self,
        pred: &str,
        i: usize,
        col: &RefColumn,
        t: &SyntaxNode,
    ) -> L<T> {
        let p = pred.rsplit("::").next().unwrap_or(pred);
        let column = match &col.name {
            Some(n) => format!("`{p}`'s column `{n}`"),
            None => format!("`{p}`'s column {}", i + 1),
        };
        let text = t.text().to_string();
        let name = tokens(t)
            .find(|x| x.kind() == STRING)
            .and_then(|x| unescape(x.text()).ok());
        let help = match (col.typ(), name) {
            (Some(typ), Some(a)) => format!(
                "write the resource: `{}`, or its name in scope",
                crate::ir::Address {
                    typ: typ.clone(),
                    name: a,
                }
            ),
            _ => "write the resource: its name in scope, or `T[\"a\"]`".to_string(),
        };
        let d = Diagnostic::error(
            self.span(t),
            format!(
                "{column} is {}: a row gives it a resource, not `{text}`",
                col.shown()
            ),
        )
        .with_help(help);
        self.diags.push(d);
        Err(Skip)
    }
}
