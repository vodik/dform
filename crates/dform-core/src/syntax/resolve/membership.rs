//! Membership over a named type's values (docs/grammar.md "Membership").
//!
//! - `x in T`, `T` an enum type alias (`type environment = enum("staging",
//!   "prod")`), binds `x` to each value in declaration order (R-70),
//!   lowered as a range is, over the fact `__enum("environment",
//!   ["staging", "prod"])` the type's declaration states: `__enum(T, L),
//!   member(L, X)`, so `why` shows the type as the leaf. `x in env`,
//!   `env` an input of an enum type, is a cell, not a type: an error that
//!   says to name the type.

use super::*;

/// The relation of the enum types `x in T` ranges over: `__enum(T,
/// [values])`, one fact at the type's declaration, so `why` names it.
pub(super) const ENUM: &str = "__enum";

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
            let Some(t) = self.input_node(rc.scope, head).and_then(|n| node(&n, TYPE_EXPR)) else {
                return Ok(None);
            };
            let first = tokens(&t).next().map(|t| t.text().to_string());
            let named = match first.as_deref() {
                Some("enum") | None => None,
                Some(_) => Some(t.text().to_string().trim().to_string()),
            };
            let members = match &named {
                Some(n) => self
                    .alias(&t, n)
                    .and_then(|x| crate::types::members(&x)),
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
        self.helpers.push(Stmt::Fact(atom_at(ENUM, vec![str_term(&name), values], def)));
        let list = var(&fresh(rc, "Values"));
        out.push(Lit::Pos(atom_at(ENUM, vec![str_term(&name), list.clone()], span)));
        if lhs.kind() == TUPLE {
            return self.pattern_in(rc, lhs, list, out, span);
        }
        let item = self.term(rc, lhs, Pos::Content, out)?;
        Ok(Lit::Pos(atom_at("member", vec![list, item], span)))
    }

    /// The declaration of the input `name` in scope.
    fn input_node(&self, scope: usize, name: &str) -> Option<SyntaxNode> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.decls.scopes[s].input_nodes.get(name).cloned())
    }
}
