//! A resolved chain's reads (R-211 step 7): each read named once per
//! rule (by its relation, its columns and how each name in them is
//! reached), planned (`realize.rs`) and built as its node, its reads'
//! literals the resolver's own; the reads a `has` settles out of what it
//! tests bound before it; the read a literal tests or binds in place, and
//! the mark of a `has` of a resource's attribute path (R-106).

use super::*;

impl Lowerer<'_> {
    /// Hoist the read `read`, `pred(args.., V)` (V at `out`), once per
    /// rule: its node, the read into a new variable the first time, its
    /// literal into `pre`; the variable after.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn read_var(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        read: PlanRead,
        out: usize,
        hint: &str,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> ExprId {
        let args = self.read_args(&read);
        let (name, new) = self.hoist_read(rc, pred, &args, out, hint);
        if !new {
            return self.var_node(rc, span, &name);
        }
        let id = self.building(rc, span, |b| b.read_into(read.of(), &name));
        self.hoisted_into(id, pre);
        id
    }

    /// The columns of the read `read` but its value's, each as a rule's
    /// reads are kept by it ([`Self::read_key_of`]): what names it once per
    /// rule.
    pub(super) fn read_args(&self, read: &PlanRead) -> Vec<String> {
        let text = |s: &str| format!("{:?}", str_term(s));
        match read {
            PlanRead::Value(..) => Vec::new(),
            PlanRead::Attr { typ, addr, attr } => {
                vec![self.read_key_of(*typ), self.read_key_of(*addr), text(attr)]
            }
            PlanRead::Output { copy, key, .. } => vec![self.read_key_of(*copy), text(key)],
            PlanRead::World { typ, addr, path } => {
                vec![text(typ), self.read_key_of(*addr), text(path)]
            }
            PlanRead::Lookup { args, .. } => args.iter().map(|a| self.read_key_of(*a)).collect(),
        }
    }

    /// What a rule's reads are kept by for the term `id`: the term it
    /// lowers to and how each name in it is reached, so two names written
    /// alike and reached two ways (`repo`, `super.repo`) are two.
    pub(super) fn read_key_of(&self, id: ExprId) -> String {
        let mut reaches = Vec::new();
        let mut walk = vec![id];
        while let Some(e) = walk.pop() {
            let x = &self.program.exprs[e];
            if let ExprKind::Scoped { via, .. } = &x.kind {
                reaches.push(format!("{via:?}"));
            }
            walk.extend(x.kind.children());
        }
        format!("{:?} {}", self.term_of(id), reaches.join(" "))
    }

    /// The variable the read `pred(args.., V)` (V at `out`) binds, once
    /// per rule, and whether it is read here first: what it reads kept
    /// for the program's builder.
    pub(super) fn hoist_read(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        args: &[String],
        out: usize,
        hint: &str,
    ) -> (String, bool) {
        let key = format!("{pred}{args:?}@{out}");
        if let Some(Term::Var(v)) = rc.reads.get(&key) {
            return (v.clone(), false);
        }
        let name = fresh(rc, &capitalise(hint));
        rc.reads.insert(key, var(&name));
        (name, true)
    }

    /// A resolved chain as its node at `pos` (R-211 step 7): what it reads
    /// in term position its read's node, a path into it the steps, each
    /// read's variable named once per rule, over the nodes of what
    /// resolved it; of the reads resolving it, `local` (the literals the
    /// resolver made of them), those no node holds into `pre`.
    pub(super) fn realize_expr(
        &mut self,
        rc: &mut Rc,
        res: Res,
        pos: Pos,
        (local, pre): (Vec<Lit>, &mut Vec<Lit>),
        span: Span,
    ) -> L<ExprId> {
        let plan = self.plan(rc, res, pos, span)?;
        let (id, left) = self.building(rc, span, |b| {
            let id = plan.build(b);
            (id, b.not_held(id, local))
        });
        pre.extend(left);
        Ok(id)
    }

    /// The goals binding the reads [`Self::settled`] takes out of `res`'s
    /// parts, each `x = read`: what holds the settled `res` holds them
    /// before it.
    pub(super) fn settled_reads(&mut self, rc: &Rc, res: &Res, span: Span) -> Vec<GoalId> {
        let segs = |path: &[Seg]| -> Vec<ExprId> {
            path.iter()
                .filter_map(|s| match s {
                    Seg::I(x) | Seg::K(x) => Some(*x),
                    Seg::F(_) => None,
                })
                .collect()
        };
        let parts = match res {
            Res::Ref { addr, path, .. } => [vec![*addr], segs(path)].concat(),
            Res::Var { var, path } => [vec![*var], segs(path)].concat(),
            Res::Value { path, .. } => segs(path),
            _ => Vec::new(),
        };
        self.building(rc, span, |b| {
            parts.into_iter().flat_map(|p| b.settled_reads(p)).collect()
        })
    }

    /// `goal` after the goals `before`: a group of them, at `span`.
    pub(super) fn after(
        &mut self,
        rc: &Rc,
        span: Span,
        before: Vec<GoalId>,
        goal: GoalId,
    ) -> GoalId {
        if before.is_empty() {
            return goal;
        }
        let mut goals = before;
        goals.push(goal);
        let after = Vec::new();
        self.goal(rc, span, GoalKind::Group { goals, after })
    }

    /// `res` with the reads its parts make left where they were made: each
    /// part that holds a read is the term it lowered to, its reads'
    /// variables in their place.
    pub(super) fn settled(&mut self, rc: &Rc, res: Res, span: Span) -> Res {
        let settle = |l: &mut Self, id: ExprId| l.building(rc, span, |b| b.settled(id));
        let segs = |l: &mut Self, path: Vec<Seg>| -> Vec<Seg> {
            path.into_iter()
                .map(|s| match s {
                    Seg::I(i) => Seg::I(settle(l, i)),
                    Seg::K(k) => Seg::K(settle(l, k)),
                    f => f,
                })
                .collect()
        };
        match res {
            Res::Ref { typ, addr, path } => Res::Ref {
                typ,
                addr: settle(self, addr),
                path: segs(self, path),
            },
            Res::Var { var, path } => Res::Var {
                var: settle(self, var),
                path: segs(self, path),
            },
            Res::Value { decl, via, path } => Res::Value {
                decl,
                via,
                path: segs(self, path),
            },
            res => res,
        }
    }

    /// What a resolved chain at `pos` reads and the path into it, each
    /// variable named in the order the reads are made.
    pub(super) fn plan(&mut self, rc: &mut Rc, res: Res, pos: Pos, span: Span) -> L<Plan> {
        Ok(match res {
            Res::Val(t) => Plan::Node(t),
            Res::Type(t) => Plan::Node(self.str_node(span, &t)),
            Res::Var { var: v, path } => self.plan_path(rc, Plan::Node(v), path, None),
            Res::Ref { typ, addr, path } if path.is_empty() => Plan::Node(match pos {
                Pos::Value => {
                    let empty = self.str_node(span, "");
                    let args = vec![typ, addr, empty];
                    self.building(rc, span, |b| b.form(crate::address::REF, args))
                }
                _ => addr,
            }),
            Res::Ref { typ, addr, path } if pos != Pos::Content => {
                let Some(p) = self.path_string(&path) else {
                    return self.error(span, "a reference's path is constant");
                };
                let p = self.str_node(span, &p);
                let args = vec![typ, addr, p];
                Plan::Node(self.building(rc, span, |b| b.form(crate::address::REF, args)))
            }
            Res::Ref { typ, addr, path } => {
                let Seg::F(first) = &path[0] else {
                    return self.error(span, "a resource's attribute is `r.name`");
                };
                let first = crate::address::path_key(first).into_owned();
                let read = PlanRead::Attr {
                    typ,
                    addr,
                    attr: first.clone(),
                };
                let base = self.plan_read(rc, "attr", (3, &first), read, span);
                let at = (self.leaf_copy(typ), first);
                self.plan_path(rc, base, path[1..].to_vec(), Some(at))
            }
            Res::Output {
                inst,
                key,
                path,
                typ,
                of,
            } => {
                let read = PlanRead::Output {
                    copy: inst,
                    key: key.clone(),
                    of,
                };
                let base = self.plan_read(rc, "output", (2, &key), read, span);
                match typ {
                    // A typed output holds an address: given as a value, it
                    // is the reference (R-43).
                    Some(typ) if pos == Pos::Value && path.is_empty() => Plan::RefTo {
                        typ,
                        addr: Box::new(base),
                    },
                    _ => self.plan_path(rc, base, path, None),
                }
            }
            Res::World { typ, addr, path } => {
                let last = path.rsplit('.').next().unwrap_or(&path).to_string();
                let read = PlanRead::World { typ, addr, path };
                self.plan_read(rc, "cloud_attr", (3, &last), read, span)
            }
            Res::Value { decl, via, path } => {
                let base = self.value_plan(rc, decl, via, span);
                self.plan_path(rc, base, path, None)
            }
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } => {
                let hint = pred.replace('.', "_");
                let read = PlanRead::Lookup {
                    rel: pred.clone(),
                    args,
                    out,
                };
                let base = self.plan_read(rc, &pred, (out, &hint), read, span);
                self.plan_path(rc, base, path, None)
            }
        })
    }

    /// The value `decl` read `via`: its variable when the rule read it
    /// already, else the read into a new one, named after the value (a
    /// used module's after its relation, `m::k`).
    pub(super) fn value_plan(
        &mut self,
        rc: &mut Rc,
        decl: DeclRef,
        via: Reach,
        span: Span,
    ) -> Plan {
        let key = read_key(&decl.name, &via);
        if let Some(v) = rc.values.get(&key) {
            let v = v.clone();
            return Plan::Node(self.var_node(rc, span, &v));
        }
        let pred = via.relation(&decl.name);
        let used = matches!(via, Reach::Use(_));
        let name = fresh(rc, &capitalise(if used { &pred } else { &decl.name }));
        rc.values.insert(key, name.clone());
        Plan::Read {
            read: PlanRead::Value(decl, via),
            var: name,
        }
    }

    /// The read `read`, `pred(args.., V)` (V at `out`): its variable when
    /// the rule read it already, else the read into a new one.
    pub(super) fn plan_read(
        &mut self,
        rc: &mut Rc,
        pred: &str,
        (out, hint): (usize, &str),
        read: PlanRead,
        span: Span,
    ) -> Plan {
        let args = self.read_args(&read);
        match self.hoist_read(rc, pred, &args, out, hint) {
            (v, false) => Plan::Node(self.var_node(rc, span, &v)),
            (v, true) => Plan::Read { read, var: v },
        }
    }

    /// Fields and indexes into `base`. From the attribute `at` (the
    /// resource's type and the attribute's top segment) of a resource, an
    /// element by its key, `containers["api"]`, is the element of the
    /// keyed list whose one key field (`type_list_key`) has that value
    /// (R-35); elsewhere a key is a position.
    pub(super) fn plan_path(
        &mut self,
        rc: &mut Rc,
        base: Plan,
        path: Vec<Seg>,
        mut at: Option<(ExprId, String)>,
    ) -> Plan {
        let mut steps = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        let flush = |steps: &mut Vec<PlanStep>, fields: &mut Vec<String>| {
            if fields.is_empty() {
                return;
            }
            let p = fields
                .iter()
                .map(|f| crate::address::path_key(f))
                .collect::<Vec<_>>()
                .join(".");
            fields.clear();
            steps.push(PlanStep::Fields(p));
        };
        for s in path {
            match s {
                Seg::F(f) => {
                    if let Some((_, list)) = &mut at {
                        list.push('.');
                        list.push_str(&crate::address::path_key(&f));
                    }
                    fields.push(f)
                }
                Seg::K(key) if at.is_some() => {
                    let (typ, list) = at.take().expect("matched");
                    flush(&mut steps, &mut fields);
                    let item = fresh(rc, "Item");
                    let keys = fresh(rc, "Keys");
                    let field = fresh(rc, "Key");
                    steps.push(PlanStep::Keyed {
                        key,
                        typ,
                        list,
                        item,
                        keys,
                        field,
                    });
                }
                Seg::I(i) | Seg::K(i) => {
                    at = None;
                    flush(&mut steps, &mut fields);
                    let item = fresh(rc, "Item");
                    steps.push(PlanStep::Element { i, item });
                }
            }
        }
        flush(&mut steps, &mut fields);
        match steps.is_empty() {
            true => base,
            false => Plan::Path {
                base: Box::new(base),
                steps,
            },
        }
    }

    /// The read of a value hoisted, `k(V)`: its node, the read into
    /// `var_name`, its literal into `pre`.
    pub(super) fn value_read(
        &mut self,
        rc: &Rc,
        var_name: &str,
        (decl, via): (DeclRef, Reach),
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> ExprId {
        let read = ReadOf::Value(decl, via);
        let id = self.building(rc, span, |b| b.read_into(read, var_name));
        self.hoisted_into(id, pre);
        id
    }

    /// The read a resolved chain is, with `value` in its value column: the
    /// direct forms `R.p == c`, `k == c`, `not R.p`. `None` when the chain
    /// is not one read.
    pub(super) fn read_atom(&self, res: &Res, value: Term, span: Span) -> Option<Atom> {
        let t = |id: &ExprId| self.term_of(*id);
        match res {
            Res::Ref { typ, addr, path } if path.len() == 1 => match &path[0] {
                Seg::F(p) => Some(atom_at(
                    "attr",
                    vec![
                        t(typ),
                        t(addr),
                        str_term(&crate::address::path_key(p)),
                        value,
                    ],
                    span,
                )),
                Seg::I(_) | Seg::K(_) => None,
            },
            Res::Output {
                inst, key, path, ..
            } if path.is_empty() => {
                Some(atom_at("output", vec![t(inst), str_term(key), value], span))
            }
            Res::World { typ, addr, path } => Some(atom_at(
                "cloud_attr",
                vec![str_term(typ), t(addr), str_term(path), value],
                span,
            )),
            Res::Value { decl, via, path } if path.is_empty() => {
                Some(atom_at(&via.relation(&decl.name), vec![value], span))
            }
            Res::Lookup {
                pred,
                args,
                out,
                path,
            } if path.is_empty() => {
                let mut args: Vec<Term> = args.iter().map(t).collect();
                args.insert(*out, value);
                Some(atom_at(pred, args, span))
            }
            _ => None,
        }
    }

    /// `has r.PATH` of a resource's attribute (a path of fields):
    /// `__has(T, A, "PATH")`, which the compiler answers from the schema in
    /// a rule that writes under PATH ([`mark_has`]).
    pub(super) fn has_atom(&self, res: &Res, span: Span) -> Option<Atom> {
        let m = self.has_mark(res)?;
        Some(atom_at(
            crate::partition::HAS,
            vec![self.term_of(m.typ), self.term_of(m.addr), str_term(&m.path)],
            span,
        ))
    }

    /// The mark of `has r.PATH` ([`Self::has_atom`]), its parts the nodes
    /// of what resolved `r`.
    pub(super) fn has_mark(&self, res: &Res) -> Option<HasMark> {
        let Res::Ref { typ, addr, path } = res else {
            return None;
        };
        let keys: Vec<String> = path
            .iter()
            .map(|s| match s {
                Seg::F(p) => Some(crate::address::path_key(p).into_owned()),
                Seg::I(_) | Seg::K(_) => None,
            })
            .collect::<Option<_>>()?;
        (!keys.is_empty()).then(|| HasMark {
            typ: *typ,
            addr: *addr,
            path: keys.join("."),
        })
    }

    /// A constant path as a stored path: `a.b[0].c`, `a."b.c"`.
    pub(super) fn path_string(&self, path: &[Seg]) -> Option<String> {
        let mut out = String::new();
        for s in path {
            match s {
                Seg::F(f) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(&crate::address::path_key(f));
                }
                Seg::I(i) => match (&self.program.exprs[*i].kind, self.program.exprs[*i].hoisted) {
                    (ExprKind::Lit(Value::Int(i)), None) => out.push_str(&format!("[{i}]")),
                    _ => return None,
                },
                Seg::K(_) => return None,
            }
        }
        Some(out)
    }
}
