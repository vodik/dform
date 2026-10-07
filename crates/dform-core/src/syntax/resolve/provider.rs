//! A provider's `use NAME { .. }` (docs/grammar.md "Providers"). `source` is
//! a constant: the stack reads it to start the provider. Every other
//! setting is configuration, read like any rule reads (inputs, settings
//! rows, value names, tables, `env.var`), so a keyed deployment configures
//! its providers by its key:
//!
//! ```text
//! use NAME { k1 = t1, k2 = t2 }        provider_config("NAME", { k1: t1', k2: t2' }) :- reads
//! expect_account = t                   provider_expect_account("NAME", t') :- reads
//! ```
//!
//! `provider_config` is what the engine configures a provider from
//! (`Providers::configure_from`); `provider_expect_account` is what it
//! checks the account the provider reports against
//! (`Providers::check_accounts`).
//!
//! A provider's `use` block also brings the provider's externs into scope
//! (DESIGN.org R-8): a built-in fact provider's (`file`, `env`, `time`,
//! `externs::BUILTINS`) are declared here, and a program that writes
//! `extern` for one is told to write the provider's `use` instead.
//! `memo.first` (R-60) is in scope with no provider's `use`. `random`
//! is no provider: its functions are std's (`std/random.df`).

use super::*;

/// The `env` provider's extern `env.var(+name, -value: secret(string))`:
/// the process environment's variable, a secret, never persisted.
pub const ENV_VAR: &str = "env.var";

/// The term calls of a built-in extern, its last column read: `env.var(N)`,
/// `time.now()`, `memo.first(K, C)`, `ssh.read(H, U, P)`.
const TERM_CALLS: [&str; 4] = [
    ENV_VAR,
    crate::externs::TIME_NOW,
    crate::memo::FIRST,
    crate::plugin::ssh::READ,
];

/// A provider's `use` block's setting that is checked, not sent.
const EXPECT_ACCOUNT: &str = "expect_account";

impl Lowerer<'_> {
    pub(super) fn provider(&mut self, n: &SyntaxNode, scope: usize, outer: &Rc) -> L<Vec<Stmt>> {
        let span = self.span(n);
        // `use ovh as ca` (R-115): the provider `ovh` under the name `ca`,
        // its own process, settings and state, its types `ca.instance`.
        let (of, name) = use_parts(n);
        if of == "random" {
            let d = Diagnostic::error(span, "random is not a provider").with_help(
                "random.password, random.bytes, random.id, random.uuid and \
                     random.signing_key are std functions (std/random.df): delete the \
                     `use random` statement and call them",
            );
            self.diags.push(d);
            return Err(Skip);
        }
        if of != name && crate::externs::builtin(&of).is_some_and(|b| b.in_process) {
            return self.error(
                span,
                format!(
                    "`use {of} as {name}`: {of} is answered by dform itself, with nothing to \
                     configure twice: write `use {of}`"
                ),
            );
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
            let group = format!("use {name}");
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
                        of: (of != name).then(|| of.clone()),
                        config: source,
                        span,
                    }),
                );
                if others.len() > 1 {
                    let sites: Vec<(String, Span)> = others
                        .iter()
                        .map(|o| (format!("use {name}"), self.span(o)))
                        .collect();
                    out.extend(crate::modules::denies(&format!("use {name}"), &sites));
                }
            }
        }
        Ok(out)
    }

    /// The provider's `use`s beside `n` that bind `name` (`use ovh as
    /// ca` binds `ca`), in source order. A name is
    /// declared once, or several times each under a clause (R-104); the
    /// scope's one namespace says so (`redeclared`).
    fn providers_named(&mut self, n: &SyntaxNode, name: &str) -> L<Vec<SyntaxNode>> {
        Ok(n.parent()
            .into_iter()
            .flat_map(|p| p.children())
            .filter(|c| provider_use(c, self.units, &self.decls.deployed).is_some())
            .filter(|c| use_parts(c).1 == name)
            .collect())
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
    /// provider's extern, or of one with no provider's `use` for it,
    /// is an error naming the statement to write.
    pub(super) fn env_var_call(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
        pre: &mut Vec<Lit>,
    ) -> Option<L<Term>> {
        let name = self.callee(n)?;
        if name == "ssh.run" {
            let d = Diagnostic::error(
                self.span(n),
                "`ssh.run` is not a function: a command is a provider's apply",
            )
            .with_help(
                "the ssh provider reads a remote filesystem: `ssh.read(host, user, path)`; \
                 a file, a package or a unit to manage is a resource of a provider whose \
                 apply runs what it must",
            );
            self.diags.push(d);
            return Some(Err(Skip));
        }
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
                format!("{name} is the {head} provider's: declare `use {head}`"),
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

    /// The externs of the built-in fact providers the program's `use`s
    /// name, declared: their statements.
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
                // Declared where the provider's `use` stands.
                f.span = span;
                let cols = f.args.iter().map(|b| (b.input, b.name.clone())).collect();
                self.decls.externs.insert(f.name.clone(), cols);
                out.push(Stmt::ExternFn(f));
            }
        }
        out
    }

    /// The program's providers' `use`s: name and span, the first of a
    /// name.
    fn provider_blocks(&self) -> Vec<(String, Span)> {
        let mut out: Vec<(String, Span)> = Vec::new();
        for u in self.units {
            for (c, name) in u
                .root
                .children()
                .filter_map(|c| provider_use(&c, self.units, &self.decls.deployed).map(|n| (c, n)))
            {
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
                .with_label(at, format!("the {head} provider is declared here"))
                .with_help("delete the `extern` statement"),
            None => d.with_help(format!("write `use {head}` instead")),
        };
        self.diags.push(d);
        Err(Skip)
    }
}
