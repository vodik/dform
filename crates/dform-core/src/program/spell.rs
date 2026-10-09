//! Any node in the program's words (R-211): `x in r`, `p.spec.replicas`,
//! `restic(name, "/data")`, `"${x}"`, `storage in 1Gi..=500Gi`. What a
//! message says of a node with no source text of its own (an instanced
//! read, a helper spelled through its origin) comes from here; where the
//! exact text matters, the source by the node's span is cheaper.
//!
//! An opaque item is spelled as today: a rule or a fact by
//! `crate::spell`'s core form, any other statement by its header.

use super::node::*;
use super::{NodeId, Program};
use crate::ast::{FieldOp, Rank, Stmt, TypeExpr};
use crate::inputs::type_text;

/// `id` as the program writes it.
pub fn spell(program: &Program, id: NodeId) -> String {
    let s = Speller { p: program };
    match id {
        NodeId::Item(i) => s.item(i),
        NodeId::Expr(e) => s.expr(e),
        NodeId::Goal(g) => s.goal(g),
        NodeId::Pattern(p) => s.pattern(p),
        NodeId::Clause(c) => s.clause(c),
    }
}

/// What kind of statement an item is, in a word or two.
pub fn kind(k: &ItemKind) -> &'static str {
    match k {
        ItemKind::Opaque(_) => "a statement",
        ItemKind::Module {
            component: true, ..
        } => "a component",
        ItemKind::Module { .. } => "a module",
        ItemKind::Let { .. } | ItemKind::LetFn { .. } => "a let",
        ItemKind::Rule { .. } => "a rule",
        ItemKind::Check {
            kind: CheckKind::Deny,
            ..
        } => "a deny",
        ItemKind::Check { .. } => "a warn",
        ItemKind::Resource { .. } => "a resource",
        ItemKind::Contribute { .. } | ItemKind::SetFrom { .. } => "a set",
        ItemKind::Copy { .. } => "a copy",
        ItemKind::Input { .. } | ItemKind::RelationInput { .. } => "an input",
        ItemKind::Output { .. } | ItemKind::OutputRelation { .. } => "an output",
        ItemKind::Provider { .. } => "a provider",
        ItemKind::Decl { .. } => "a decl",
        ItemKind::Extern { .. } => "an extern",
        ItemKind::TypeBlock { .. } => "a type block",
        ItemKind::Doc { .. } => "a doc comment",
    }
}

struct Speller<'p> {
    p: &'p Program,
}

fn join(xs: impl IntoIterator<Item = String>, sep: &str) -> String {
    xs.into_iter().collect::<Vec<_>>().join(sep)
}

fn rank(r: Option<Rank>) -> &'static str {
    match r {
        Some(Rank::Default) => " @default",
        Some(Rank::Override) => " @override",
        Some(Rank::Normal) | None => "",
    }
}

fn op(o: FieldOp) -> &'static str {
    match o {
        FieldOp::Assign => "=",
        FieldOp::Add => "+=",
    }
}

fn typed(ty: &Option<TypeExpr>) -> String {
    ty.as_ref()
        .map_or(String::new(), |t| format!(": {}", type_text(t)))
}

impl Speller<'_> {
    fn item(&self, id: ItemId) -> String {
        match &self.p.items[id].kind {
            ItemKind::Opaque(s) => join(s.iter().map(opaque), "\n"),
            ItemKind::Module {
                path, component, ..
            } => match component {
                true => format!("component {path}"),
                false => format!("module {path}"),
            },
            ItemKind::Let {
                name,
                ty,
                value,
                clause,
                rank: r,
                ..
            } => format!(
                "let {name}{} = {}{}{}",
                typed(ty),
                self.expr(*value),
                rank(*r),
                self.where_(*clause)
            ),
            ItemKind::LetFn {
                name,
                params,
                value,
                clause,
            } => {
                let params = params.iter().map(|p| {
                    let d = p
                        .default
                        .map_or(String::new(), |d| format!(" = {}", self.expr(d)));
                    format!("{}{}{d}", self.var(p.var), typed(&p.ty))
                });
                format!(
                    "let {name}({}) = {}{}",
                    join(params, ", "),
                    self.expr(*value),
                    self.where_(*clause)
                )
            }
            ItemKind::Rule {
                head,
                clause,
                rank: r,
            } => format!(
                "{}{}{}",
                self.rel(&head.rel, &head.args),
                rank(*r),
                self.where_(*clause)
            ),
            ItemKind::Check {
                kind,
                message,
                detail,
                clause,
            } => {
                let word = match kind {
                    CheckKind::Deny => "deny",
                    CheckKind::Warn => "warn",
                };
                let detail = detail.map_or(String::new(), |d| format!(" {}", self.expr(d)));
                format!(
                    "{word} {}{detail}{}",
                    self.expr(*message),
                    self.where_(*clause)
                )
            }
            ItemKind::Resource {
                typ,
                name,
                rank: r,
                body,
                clause,
            } => {
                let body = match body {
                    ResourceBody::Block(es) => format!(
                        " {{ {} }}",
                        join(
                            es.iter().map(|e| format!(
                                "{} {} {}{}",
                                self.steps(&e.path).trim_start_matches('.'),
                                op(e.op),
                                self.expr(e.value),
                                rank(e.rank)
                            )),
                            ", "
                        )
                    ),
                    ResourceBody::Value(v) => format!(" = {}", self.expr(*v)),
                };
                format!(
                    "resource {} {}{}{body}{}",
                    typ.name,
                    self.header(name),
                    rank(*r),
                    self.where_(*clause)
                )
            }
            ItemKind::Contribute {
                target,
                op: o,
                value,
                rank: r,
                clause,
                ..
            } => format!(
                "set {} {} {}{}{}",
                self.target(target),
                op(*o),
                self.expr(*value),
                rank(*r),
                self.where_(*clause)
            ),
            ItemKind::SetFrom {
                doc,
                rank: r,
                clause,
            } => format!(
                "set from {}{}{}",
                self.expr(*doc),
                rank(*r),
                self.where_(*clause)
            ),
            ItemKind::Copy {
                kind,
                def,
                name,
                inputs,
                clause,
                ..
            } => {
                let block = match inputs.is_empty() {
                    true => String::new(),
                    false => format!(
                        " {{ {} }}",
                        join(
                            inputs
                                .iter()
                                .map(|(k, _, v)| format!("{k} = {}", self.expr(*v))),
                            ", "
                        )
                    ),
                };
                let head = match kind {
                    CopyKind::Use => format!("use {} as {}", def.name, self.header(name)),
                    CopyKind::Component | CopyKind::Deployment => {
                        format!("resource {} {}", def.name, self.header(name))
                    }
                };
                format!("{head}{block}{}", self.where_(*clause))
            }
            ItemKind::Input {
                name,
                key,
                ty,
                default,
                refinement,
                guard,
                ..
            } => format!(
                "{} {name}: {}{}{}{}",
                if *key { "key" } else { "input" },
                type_text(ty),
                default.map_or(String::new(), |d| format!(" = {}", self.expr(d))),
                refinement.map_or(String::new(), |c| format!(" check {}", self.clause(c))),
                self.where_(*guard)
            ),
            ItemKind::RelationInput {
                rel,
                source,
                clause,
            } => format!(
                "input {}{}{}",
                rel.name,
                source.map_or(String::new(), |s| format!(" from {}", self.expr(s))),
                self.where_(*clause)
            ),
            ItemKind::Output {
                name,
                ty,
                value,
                clause,
            } => format!(
                "output {name}{}{}{}",
                typed(ty),
                value.map_or(String::new(), |v| format!(" = {}", self.expr(v))),
                self.where_(*clause)
            ),
            ItemKind::OutputRelation { rel, .. } => format!("output {}", rel.name),
            ItemKind::Provider { name, of, .. } => match of {
                Some(of) => format!("provider {of} as {name}"),
                None => format!("provider {name}"),
            },
            ItemKind::Decl {
                rel,
                columns,
                mixed,
            } => format!(
                "decl {}({}){}",
                rel.name,
                join(
                    columns.iter().map(|(n, t)| format!("{n}{}", typed(t))),
                    ", "
                ),
                if *mixed { " mixed" } else { "" }
            ),
            ItemKind::Extern { name, args } => format!(
                "extern {name}({})",
                join(
                    args.iter().map(|a| format!(
                        "{}{}{}",
                        if a.input { "+" } else { "-" },
                        a.name,
                        typed(&a.ty)
                    )),
                    ", "
                )
            ),
            ItemKind::TypeBlock { name, .. } => format!("type {name}"),
            ItemKind::Doc { kind, name, .. } => format!("doc of {kind} {name}"),
        }
    }

    fn where_(&self, c: Option<ClauseId>) -> String {
        c.map_or(String::new(), |c| format!(" where {}", self.clause(c)))
    }

    fn clause(&self, id: ClauseId) -> String {
        join(self.p.clauses[id].goals.iter().map(|g| self.goal(*g)), ", ")
    }

    fn goal(&self, id: GoalId) -> String {
        match &self.p.goals[id].kind {
            GoalKind::Rel { rel, args } => self.rel(rel, args),
            GoalKind::Member { pat, coll } => {
                format!("{} in {}", self.pattern(*pat), self.coll(coll))
            }
            GoalKind::Bind { pat, value } => {
                format!("{} = {}", self.pattern(*pat), self.expr(*value))
            }
            GoalKind::Compare { lhs, ops } => {
                let mut out = self.expr(*lhs);
                for (o, e) in ops {
                    out.push_str(&format!(" {} {}", cmp(*o), self.expr(*e)));
                }
                out
            }
            GoalKind::Has(h) => match h {
                Has::Resource { typ, addr } => {
                    format!("has {}[{}]", self.expr(*typ), self.expr(*addr))
                }
                Has::Read(e) | Has::Walk { value: e, .. } => format!("has {}", self.expr(*e)),
            },
            GoalKind::Truth(e) => self.expr(*e),
            GoalKind::Hoisted { goals, .. } => join(goals.iter().map(|g| self.goal(*g)), ", "),
            GoalKind::Marked { goal, .. } => self.goal(*goal),
            GoalKind::Not { clause, .. } => {
                let goals = &self.p.clauses[*clause].goals;
                match goals.as_slice() {
                    [g] => match &self.p.goals[*g].kind {
                        GoalKind::Member { pat, coll } => {
                            format!("{} not in {}", self.pattern(*pat), self.coll(coll))
                        }
                        _ => format!("not {}", self.goal(*g)),
                    },
                    _ => format!("not {{ {} }}", self.clause(*clause)),
                }
            }
            GoalKind::Fold { var, agg, .. } => format!("{} = {}", self.var(*var), self.expr(*agg)),
        }
    }

    fn rel(&self, rel: &RelRef, args: &RelArgs) -> String {
        let args = match args {
            RelArgs::Positional(ps) => join(ps.iter().map(|p| self.pattern(*p)), ", "),
            RelArgs::Record(fs) => join(
                fs.iter().map(|(n, p)| format!("{n}: {}", self.pattern(*p))),
                ", ",
            ),
        };
        format!("{}({args})", rel.name)
    }

    fn coll(&self, c: &Coll) -> String {
        match c {
            Coll::Expr(e) | Coll::Type(e) | Coll::Each(e) | Coll::TypeOf(e) => self.expr(*e),
            Coll::ProviderType { typ: n, .. }
            | Coll::Enum { name: n, .. }
            | Coll::Namespace { ns: n, .. }
            | Coll::Copies { component: n, .. } => n.clone(),
            Coll::World(t) => format!("world.{t}"),
        }
    }

    fn pattern(&self, id: PatternId) -> String {
        match &self.p.patterns[id].kind {
            PatternKind::Hole => "_".into(),
            PatternKind::Bind(v) => self.var(*v),
            PatternKind::Expr(e) => self.expr(*e),
            PatternKind::Tuple { elems, rest } => {
                let mut parts: Vec<String> = elems.iter().map(|p| self.pattern(*p)).collect();
                parts.extend(rest.map(|r| format!("..{}", self.var(r))));
                format!("({})", parts.join(", "))
            }
            PatternKind::Object { fields, rest } => {
                let mut parts: Vec<String> = fields
                    .iter()
                    .map(|(n, _, p)| match &self.p.patterns[*p].kind {
                        PatternKind::Bind(v) if self.p.vars[*v].name == *n => n.clone(),
                        _ => format!("{n}: {}", self.pattern(*p)),
                    })
                    .collect();
                parts.extend(rest.map(|r| format!("..{}", self.var(r))));
                format!("{{ {} }}", parts.join(", "))
            }
        }
    }

    fn var(&self, id: VarId) -> String {
        self.p.vars[id].name.clone()
    }

    fn expr(&self, id: ExprId) -> String {
        match &self.p.exprs[id].kind {
            ExprKind::Missing | ExprKind::Hole => "_".into(),
            ExprKind::Hoisted { value, .. } => self.expr(*value),
            ExprKind::Read { goal, .. } => self.goal(*goal),
            ExprKind::Lit(v) => crate::spell::value(v),
            ExprKind::Quantity { text } => text.clone(),
            ExprKind::Var(v) => self.var(*v),
            ExprKind::Value(d) => d.name.clone(),
            ExprKind::Resource { typ, addr } => format!("{}[{}]", typ.name, self.expr(*addr)),
            ExprKind::RefOf(e) => format!("ref({})", self.expr(*e)),
            ExprKind::Field { base, path } => format!("{}{}", self.expr(*base), self.steps(path)),
            ExprKind::Output { copy, key } => format!("{}.{key}", self.expr(*copy)),
            ExprKind::Deployed { stack, keys, out } => {
                let keys = keys.iter().map(|(k, e, pun)| match pun {
                    true => k.clone(),
                    false => format!("{k} = {}", self.expr(*e)),
                });
                format!("{}[{}].{out}", stack.name, join(keys, ", "))
            }
            ExprKind::World { typ, addr, path } => format!(
                "world.{}[{}]{}",
                typ.name,
                self.expr(*addr),
                self.steps(path)
            ),
            ExprKind::Setting { key } => format!("settings.{key}"),
            ExprKind::Lookup { rel, args, path } => format!(
                "{}[{}]{}",
                rel.name,
                join(args.iter().map(|a| self.expr(*a)), ", "),
                self.steps(path)
            ),
            ExprKind::Call { callee, args } => {
                let name = match callee {
                    Callee::Std(f) => f.name.clone(),
                    Callee::Let(i) => match &self.p.items[*i].kind {
                        ItemKind::LetFn { name, .. } => name.clone(),
                        k => kind(k).into(),
                    },
                    Callee::Extern(r) => r.name.clone(),
                    Callee::Loader(n) | Callee::Data(n) => n.clone(),
                };
                let args = args.iter().map(|a| match &a.name {
                    Some((n, _)) => format!("{n}: {}", self.expr(a.value)),
                    None => self.expr(a.value),
                });
                format!("{name}({})", join(args, ", "))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let op = match op {
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                    BinOp::Mod => "%",
                };
                format!("{} {op} {}", self.expr(*lhs), self.expr(*rhs))
            }
            ExprKind::Neg(e) => format!("-{}", self.expr(*e)),
            ExprKind::Interp { parts } => {
                let mut out = String::from("\"");
                for p in parts {
                    match p {
                        Piece::Text(t) => {
                            let q = crate::spell::quote(t);
                            out.push_str(&q[1..q.len() - 1]);
                        }
                        Piece::Hole(e) => out.push_str(&format!("${{{}}}", self.expr(*e))),
                    }
                }
                out.push('"');
                out
            }
            ExprKind::Object { parts } => {
                let parts = parts.iter().map(|p| match p {
                    ObjPart::Field {
                        key,
                        value,
                        pun: true,
                        ..
                    } if matches!(
                        self.p.exprs[*value].kind,
                        ExprKind::Var(_) | ExprKind::Value(_)
                    ) =>
                    {
                        key.clone()
                    }
                    ObjPart::Field { key, value, .. } => format!("{key}: {}", self.expr(*value)),
                    ObjPart::Computed { key, value } => {
                        format!("{}: {}", self.expr(*key), self.expr(*value))
                    }
                    ObjPart::Spread(e) => format!("..{}", self.expr(*e)),
                });
                format!("{{ {} }}", join(parts, ", "))
            }
            ExprKind::List { parts } => {
                let parts = parts.iter().map(|p| match p {
                    ListPart::Elem(e) => self.expr(*e),
                    ListPart::Spread(e) => format!("..{}", self.expr(*e)),
                });
                format!("[{}]", join(parts, ", "))
            }
            ExprKind::Range { lo, hi, inclusive } => format!(
                "{}{}{}",
                self.expr(*lo),
                if *inclusive { "..=" } else { ".." },
                self.expr(*hi)
            ),
            ExprKind::Comprehension { item, clause } => {
                format!("[{} | {}]", self.expr(*item), self.clause(*clause))
            }
            ExprKind::Aggregate { kind, item } => {
                format!("{}({})", aggregate_name(*kind), self.expr(*item))
            }
            ExprKind::As { value, ty } => format!("{} as {}", self.expr(*value), type_text(ty)),
            ExprKind::Type(t) => t.name.clone(),
        }
    }

    fn steps(&self, path: &[Step]) -> String {
        path.iter()
            .map(|s| match s {
                Step::Field(n, _) => format!(".{n}"),
                Step::Index(e) | Step::Key(e) => format!("[{}]", self.expr(*e)),
                Step::Each(..) => "[_]".into(),
                Step::Len => ".len".into(),
            })
            .collect()
    }

    fn target(&self, t: &Target) -> String {
        match t {
            Target::Attr { res, path } => format!("{}{}", self.expr(*res), self.steps(path)),
            Target::Input { decl, path } => {
                let mut out = decl.name.clone();
                path.iter().for_each(|p| out.push_str(&format!(".{p}")));
                out
            }
        }
    }

    fn header(&self, h: &Header) -> String {
        match h {
            Header::Bare(n) => n.clone(),
            Header::Literal(s) => crate::spell::quote(s),
            Header::Interp(e) => self.expr(*e),
        }
    }
}

fn cmp(o: CmpOp) -> &'static str {
    match o {
        CmpOp::Eq => "==",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

/// A statement the resolver lowered, as today: a rule or a fact in the
/// core form, any other by its header.
fn opaque(s: &Stmt) -> String {
    let t = crate::spell::term;
    match s {
        Stmt::Fact(a) => crate::spell::atom(a),
        Stmt::Rule(r) => crate::spell::rule(r),
        Stmt::Module(m) if m.component => format!("component {}", m.name),
        Stmt::Module(m) => format!("module {}", m.name),
        Stmt::Instance(i) => format!("resource {} {}", i.module, i.name),
        Stmt::Use(i) => format!("use {} as {}", i.module, i.name),
        Stmt::Input(i) => format!(
            "{} {}: {}",
            if i.key { "key" } else { "input" },
            i.name,
            type_text(&i.ty)
        ),
        Stmt::RelationInput(e) => format!("input {}", e.pred),
        Stmt::Output(o) => format!("output {}", o.name),
        Stmt::Provider(c) => format!("provider {}", c.name),
        Stmt::Resource(r) => format!("resource {} {}", t(&r.typ), t(&r.name)),
        Stmt::Decl(d) => format!("decl {}({})", d.pred, d.fields.join(", ")),
        Stmt::Extern(e) | Stmt::Mixed(e) | Stmt::Mode(e) => format!("decl {}/{}", e.pred, e.arity),
        Stmt::ExternFn(e) => format!("extern {}", e.name),
        Stmt::Pending(p) => {
            let crate::ast::PendingKind::TypeDecl { name, .. } = &p.kind;
            format!("type {name}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::Program;

    /// An opaque program spells each statement as `crate::spell` does
    /// today.
    #[test]
    fn an_opaque_item_spells_as_today() {
        let lowered = crate::parser::parse_program("\np(1)\nq(x) where p(x), x > 0\n").unwrap();
        let today: Vec<String> = lowered
            .statements
            .iter()
            .map(|s| match s {
                Stmt::Fact(a) => crate::spell::atom(a),
                Stmt::Rule(r) => crate::spell::rule(r),
                s => panic!("{s:?}"),
            })
            .collect();
        let program = Program::of_statements(lowered.statements);
        let spelled: Vec<String> = program
            .roots
            .iter()
            .map(|&i| spell(&program, NodeId::Item(i)))
            .collect();
        assert_eq!(spelled, today);
        assert_eq!(spelled[1], "q(X) :- p(X), X > 0");
    }
}
