//! `provider NAME { .. }` (docs/grammar.md "Provider blocks"). `source` is
//! a constant: the stack reads it to start the provider. Every other
//! setting is configuration, read like any rule reads (inputs, settings
//! rows, value names, tables, `env_var`), so a keyed deployment configures
//! its providers by its key:
//!
//! ```text
//! provider NAME { k1 = t1, k2 = t2 }   provider_config("NAME", { k1: t1', k2: t2' }) :- reads
//! expect_account = t                   provider_expect_account("NAME", t') :- reads
//! ```
//!
//! `provider_config` is what the engine configures a provider from
//! (`Providers::configure_from`); `provider_expect_account` is what it
//! checks the account the provider reports against
//! (`Providers::check_accounts`).

use super::*;

/// The builtin extern `env_var(+name, -value: secret(string))`: the
/// process environment's variable, a secret, never persisted.
pub const ENV_VAR: &str = "env_var";

/// A `provider` block's setting that is checked, not sent.
const EXPECT_ACCOUNT: &str = "expect_account";

impl Lowerer<'_> {
    pub(super) fn provider(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        let block = node(n, BLOCK);
        let mut out = Vec::new();
        let mut source = Vec::new();
        let mut settings = BTreeMap::new();
        let mut rc = self.rc(n, scope, outer);
        let mut body = Vec::new();
        if let Some(b) = &block {
            if let Some(c) = node(b, CLAUSE) {
                return self.error(self.span(&c), "a provider or stack block takes no clause");
            }
            for a in b.children().filter(|c| c.kind() == ASSIGN) {
                let at = self.span(&a);
                let key = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
                if tokens(&a).any(|t| matches!(t.kind(), PLUS_EQ | RANK)) {
                    return self.error(
                        at,
                        format!(
                            "provider {name}: a setting is `{key} = term`, with no `+=` or rank"
                        ),
                    );
                }
                let t = terms(&a).next().ok_or(Skip)?;
                match key.as_str() {
                    "source" => source.push((key, self.constant(&mut rc, &t)?, at)),
                    EXPECT_ACCOUNT => {
                        let mut rc = self.rc(&a, scope, outer);
                        let mut body = Vec::new();
                        let v = self.term(&mut rc, &t, Pos::Content, &mut body)?;
                        let head = atom_at(
                            crate::plugin::providers::EXPECT_ACCOUNT,
                            vec![str_term(&name), v],
                            at,
                        );
                        out.push(self.rule_or_fact(&rc, head, body)?);
                    }
                    _ => {
                        let v = self.term(&mut rc, &t, Pos::Content, &mut body)?;
                        if settings.insert(key.clone(), v).is_some() {
                            return self.error(at, format!("provider {name}: {key} is set twice"));
                        }
                    }
                }
            }
        }
        if !settings.is_empty() {
            let head = atom_at(
                "provider_config",
                vec![str_term(&name), Term::Obj(settings)],
                span,
            );
            out.insert(0, self.rule_or_fact(&rc, head, body)?);
        }
        out.insert(
            0,
            Stmt::Provider(Config {
                name,
                keys: Vec::new(),
                config: source,
                span,
            }),
        );
        Ok(out)
    }

    fn rule_or_fact(&mut self, rc: &Rc, head: Atom, body: Vec<Lit>) -> L<Stmt> {
        self.check_bound(rc, &body, &atom_terms(&head))?;
        Ok(if body.is_empty() {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        })
    }

    /// `env_var(NAME)` as a term: the read `env_var(NAME', V)`, `V` its
    /// value. `None`: the call is not one.
    pub(super) fn env_var_call(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> Option<L<Term>> {
        let builtin = self.decls.externs.get(ENV_VAR) == Some(&env_var_columns());
        (builtin && self.callee(n).as_deref() == Some(ENV_VAR))
            .then(|| self.env_var_read(rc, n, pos, pre))
    }

    fn env_var_read(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> L<Term> {
        let span = self.span(n);
        let args = self.bind(true, |l| l.args(rc, n, Pos::Content, pre))?;
        if args.len() != 1 {
            return self.error(span, "env_var takes one argument: the variable's name");
        }
        let res = Res::Lookup {
            pred: ENV_VAR.to_string(),
            args,
            out: 1,
            path: Vec::new(),
        };
        self.realize(rc, res, pos, pre, span)
    }

    /// Declare the builtin `env_var` when the program reads it and declares
    /// no extern of that name itself: whether it did.
    pub(super) fn declare_env_var(&mut self) -> bool {
        if self.decls.externs.contains_key(ENV_VAR) {
            return false;
        }
        let used = self.units.iter().any(|u| {
            u.root.descendants().any(|c| {
                matches!(c.kind(), CALL | CHAIN)
                    && tokens(&c).next().is_some_and(|t| t.text() == ENV_VAR)
            })
        });
        if used {
            self.decls
                .externs
                .insert(ENV_VAR.to_string(), env_var_columns());
        }
        used
    }
}

fn env_var_columns() -> Vec<(bool, String)> {
    vec![(true, "name".to_string()), (false, "value".to_string())]
}

/// The declaration of the builtin `env_var`, for a program that reads it
/// and does not declare an extern of that name itself.
pub(super) fn env_var_extern() -> Stmt {
    let string = || TypeExpr::Name("string".to_string());
    Stmt::ExternFn(ExternFn {
        name: ENV_VAR.to_string(),
        args: vec![
            BindArg {
                input: true,
                name: "name".to_string(),
                ty: Some(string()),
            },
            BindArg {
                input: false,
                name: "value".to_string(),
                ty: Some(TypeExpr::Apply("secret".to_string(), vec![string()])),
            },
        ],
        persist: false,
        span: Span::default(),
    })
}
