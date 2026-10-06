//! `provider NAME { .. }` (docs/grammar.md "Provider blocks"). `source` is
//! a constant: the stack reads it to start the provider. Every other
//! setting is configuration, read like any rule reads (inputs, settings
//! rows, value names, tables, `env.var`), so a keyed deployment configures
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
//!
//! A `provider` block also brings the provider's externs into scope
//! (DESIGN.org R-8): a built-in fact provider's (`file`, `env`, `time`,
//! `externs::BUILTINS`) are declared here, and a program that writes
//! `extern` for one is told to write the `provider` statement instead.
//! `memo.first` (R-60) is in scope with no `provider` statement. `random`
//! is no provider: its functions are std's (`std/random.df`).

use super::*;

/// The `env` provider's extern `env.var(+name, -value: secret(string))`:
/// the process environment's variable, a secret, never persisted.
pub const ENV_VAR: &str = "env.var";

/// The term calls of a built-in extern, its last column read: `env.var(N)`,
/// `time.now()`, `memo.first(K, C)`, `ssh.read(H, U, P)`, `ssh.run(H, U, C)`.
const TERM_CALLS: [&str; 5] = [
    ENV_VAR,
    crate::externs::TIME_NOW,
    crate::memo::FIRST,
    crate::plugin::ssh::READ,
    crate::plugin::ssh::RUN,
];

/// A `provider` block's setting that is checked, not sent.
const EXPECT_ACCOUNT: &str = "expect_account";

impl Lowerer<'_> {
    pub(super) fn provider(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if name == "random" {
            let d = Diagnostic::error(span, "random is not a provider").with_help(
                "random.password, random.bytes, random.id, random.uuid and \
                     random.signing_key are std functions (std/random.df): delete the \
                     `provider random` statement and call them",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        let block = node(n, BLOCK);
        let mut out = Vec::new();
        let mut source = Vec::new();
        let mut settings = BTreeMap::new();
        let mut rc = self.rc(n, scope, outer);
        // A guarded provider (R-104): its settings, its account and its
        // start hold only while the clause does.
        let clause = self.clauses(&mut rc, n)?;
        let mut body = clause.clone();
        let others = self.providers_named(n, &name)?;
        if let Some(b) = &block {
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
                match key.as_str() {
                    "source" => {
                        let Some(t) = terms(&a).next() else {
                            return self
                                .error(at, format!("provider {name}: source is a path string"));
                        };
                        source.push((key, self.constant(&mut rc, &t)?, at))
                    }
                    EXPECT_ACCOUNT => {
                        let mut rc = self.rc(&a, scope, outer);
                        let mut body = match clause.is_empty() {
                            true => Vec::new(),
                            false => self.clauses(&mut rc, n)?,
                        };
                        let v = self.entry_value(&mut rc, &a, Pos::Content, &mut body)?;
                        let head = atom_at(
                            crate::plugin::providers::EXPECT_ACCOUNT,
                            vec![str_term(&name), v],
                            at,
                        );
                        out.push(self.rule_or_fact(&rc, head, body)?);
                    }
                    _ => {
                        let v = self.entry_value(&mut rc, &a, Pos::Content, &mut body)?;
                        if settings.insert(key.clone(), v).is_some() {
                            return self.error(at, format!("provider {name}: {key} is set twice"));
                        }
                    }
                }
            }
        }
        // A guarded provider is configured by the program, with no
        // settings too: it serves nothing until its clause holds and its
        // `provider_config` fact arrives (`Providers::configure_from`).
        if !settings.is_empty() || !clause.is_empty() {
            let head = atom_at(
                "provider_config",
                vec![str_term(&name), Term::Obj(settings)],
                span,
            );
            out.insert(0, self.rule_or_fact(&rc, head, body)?);
        }
        // Declared more than once, each under a clause (R-104): the first
        // starts it, each holds while its clause does, and two that both
        // hold are the deny naming both.
        // `effects` reads each guarded declaration's clause off it.
        if !clause.is_empty() {
            let i = others.iter().position(|o| o == n).unwrap_or_default();
            let group = format!("provider {name}");
            out.push(crate::modules::declared(&group, i, clause.clone(), span));
        }
        match others.first() {
            Some(first) if first != n => {
                let first_source = node(first, BLOCK)
                    .into_iter()
                    .flat_map(|b| {
                        b.children()
                            .filter(|c| c.kind() == ASSIGN)
                            .collect::<Vec<_>>()
                    })
                    .find(|a| node(a, BLOCK_PATH).is_some_and(|p| p.text() == "source"))
                    .and_then(|a| terms(&a).next())
                    .map(|t| t.text().to_string());
                let this_source = block
                    .iter()
                    .flat_map(|b| {
                        b.children()
                            .filter(|c| c.kind() == ASSIGN)
                            .collect::<Vec<_>>()
                    })
                    .find(|a| node(a, BLOCK_PATH).is_some_and(|p| p.text() == "source"))
                    .and_then(|a| terms(&a).next())
                    .map(|t| t.text().to_string());
                if this_source.is_some() && this_source != first_source {
                    let at = self.span(first);
                    let d = Diagnostic::error(
                        span,
                        format!("provider {name}: each declaration names another source"),
                    )
                    .with_label(at, "the first declaration")
                    .with_help("one provider is started by its source: give it in the first only");
                    self.diags.push(d);
                    return Err(Skip);
                }
            }
            _ => {
                out.insert(
                    0,
                    Stmt::Provider(Config {
                        name: name.clone(),
                        config: source,
                        span,
                    }),
                );
                if others.len() > 1 {
                    let sites: Vec<(String, Span)> = others
                        .iter()
                        .map(|o| (format!("provider {name}"), self.span(o)))
                        .collect();
                    out.extend(crate::modules::denies(&format!("provider {name}"), &sites));
                }
            }
        }
        Ok(out)
    }

    /// The `provider` statements beside `n` of its name, in source order.
    /// A provider is declared once, or several times each under a clause
    /// (R-104); else the second is the error, naming the first.
    fn providers_named(&mut self, n: &SyntaxNode, name: &str) -> L<Vec<SyntaxNode>> {
        let same: Vec<SyntaxNode> = n
            .parent()
            .into_iter()
            .flat_map(|p| p.children())
            .filter(|c| c.kind() == PROVIDER && word_text(c, 1) == name)
            .collect();
        if same.len() > 1
            && same.first() != Some(n)
            && !same.iter().all(|c| node(c, CLAUSE).is_some())
        {
            let d = Diagnostic::error(
                self.span(n),
                format!("`{name}` is declared twice; give each a `where`"),
            )
            .with_label(self.span(&same[0]), "first here")
            .with_help(
                "a provider is declared once, or several times each under a clause that picks \
                 it (`provider aws { region = \"eu-west-1\" } where env == \"prod\"`)",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        Ok(same)
    }

    fn rule_or_fact(&mut self, rc: &Rc, head: Atom, body: Vec<Lit>) -> L<Stmt> {
        self.check_bound(rc, &body, &atom_terms(&head))?;
        Ok(if body.is_empty() {
            Stmt::Fact(head)
        } else {
            Stmt::Rule(RuleStmt { head, body })
        })
    }

    /// `env.var(NAME)`, `time.now()`, `memo.first(KEY, CANDIDATE)` as a
    /// term: the read of the extern with those inputs, its last column
    /// the value. `None`: the call is not one. A call of another built-in
    /// provider's extern, or of one with no `provider` statement for it,
    /// is an error naming the statement to write.
    pub(super) fn env_var_call(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> Option<L<Term>> {
        let name = self.callee(n)?;
        let (head, _) = name.split_once('.')?;
        let b = crate::externs::builtin(head)?;
        if !b.externs().iter().any(|f| f.name == name) {
            return None;
        }
        let span = self.span(n);
        if !TERM_CALLS.contains(&name.as_str()) {
            return Some(self.error(
                span,
                format!("{name} is a relation: read it as `{name}[..]` or in a body"),
            ));
        }
        if !self.decls.externs.contains_key(&name) {
            return Some(self.error(
                span,
                format!("{name} is the {head} provider's: declare `provider {head}`"),
            ));
        }
        Some(self.extern_read(rc, n, pos, pre, &name))
    }

    fn extern_read(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
        name: &str,
    ) -> L<Term> {
        let span = self.span(n);
        let args = self.bind(true, |l| l.args(rc, n, Pos::Content, pre))?;
        let want = match name {
            ENV_VAR => "one argument: the variable's name",
            crate::externs::TIME_NOW => "no argument",
            crate::plugin::ssh::READ => "three arguments: the host, the user and the path",
            crate::plugin::ssh::RUN => "three arguments: the host, the user and the command",
            _ => "two arguments: the key and the candidate",
        };
        let ins = self.decls.externs[name].len() - 1;
        if args.len() != ins {
            return self.error(span, format!("{name} takes {want}"));
        }
        let res = Res::Lookup {
            pred: name.to_string(),
            args,
            out: ins,
            path: Vec::new(),
        };
        self.realize(rc, res, pos, pre, span)
    }

    /// The externs of the built-in fact providers the program's `provider`
    /// blocks name, declared: their statements.
    pub(super) fn declare_builtin_externs(&mut self) -> Vec<Stmt> {
        let mut out = Vec::new();
        // Declared where a program names it, so a file that does not
        // (a facts file) has no extern.
        let always = crate::externs::BUILTINS
            .iter()
            .filter(|b| b.always)
            .filter(|b| {
                let call = format!("{}.", b.name);
                self.units
                    .iter()
                    .any(|u| u.root.text().to_string().contains(&call))
            })
            .map(|b| (b.name.to_string(), Span::default()));
        let blocks: Vec<(String, Span)> = always.chain(self.provider_blocks()).collect();
        for (name, span) in blocks {
            let Some(b) = crate::externs::builtin(&name) else {
                continue;
            };
            for mut f in b.externs() {
                if self.decls.externs.contains_key(&f.name) {
                    continue;
                }
                // Declared where the `provider` statement stands.
                f.span = span;
                let cols = f.args.iter().map(|b| (b.input, b.name.clone())).collect();
                self.decls.externs.insert(f.name.clone(), cols);
                out.push(Stmt::ExternFn(f));
            }
        }
        out
    }

    /// The program's `provider` statements: name and span, the first of a
    /// name.
    fn provider_blocks(&self) -> Vec<(String, Span)> {
        let mut out: Vec<(String, Span)> = Vec::new();
        for u in self.units {
            for c in u.root.children().filter(|c| c.kind() == PROVIDER) {
                let name = word_text(&c, 1);
                if out.iter().any(|(n, _)| *n == name) {
                    continue;
                }
                let r = c.text_range();
                let span = Span {
                    file: u.file,
                    start: u32::from(r.start()),
                    end: u32::from(r.end()),
                    origin: 0,
                };
                out.push((name, span));
            }
        }
        out
    }

    /// `extern NAME(..)` in a program, `NAME` a built-in fact provider's:
    /// the provider declares it.
    pub(super) fn check_extern(&mut self, name: &str, span: Span) -> L<()> {
        if self.core || self.lenient {
            return Ok(());
        }
        let Some(b) = name
            .split_once('.')
            .and_then(|(h, _)| crate::externs::builtin(h))
        else {
            return Ok(());
        };
        let head = b.name;
        let provider = self.provider_blocks().into_iter().find(|(n, _)| n == head);
        let d = Diagnostic::error(
            span,
            format!("{name} is the {head} provider's extern: a program does not declare it"),
        );
        let d = match provider {
            Some((_, at)) => d
                .with_label(at, format!("provider {head} declares it"))
                .with_help("delete the `extern` statement"),
            None => d.with_help(format!("write `provider {head}` instead")),
        };
        self.diags.push(d);
        Err(Skip)
    }
}
