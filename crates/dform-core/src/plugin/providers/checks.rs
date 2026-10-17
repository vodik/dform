//! What is refused before anything is planned: a provider configured
//! from what it serves, a type no provider declares, an account other
//! than the one the program expects.

use super::{EXPECT_ACCOUNT, Providers};
use crate::address::Address;
use crate::ast::{Atom, Lit, Term};
use crate::spell;
use crate::value::Value;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

impl Providers {
    /// The types and the externs the provider `name` serves: a mock
    /// schema's own (the mock plays several on one link), else its link's.
    fn served(&self, name: &str, i: usize) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        if let Some(spec) = self.mock_blocks.get(name) {
            let s = crate::schema::load_provider(spec)?;
            let types = s
                .provider_of
                .keys()
                .chain(s.attrs.keys().map(|(t, _)| t))
                .cloned()
                .collect();
            let externs = crate::externs::load_answers(std::slice::from_ref(spec))?
                .into_iter()
                .map(|a| a.pred.to_string())
                .collect();
            return Ok((types, externs));
        }
        let l = self.loaded();
        let of = |m: &BTreeMap<String, usize>| {
            m.iter()
                .filter(|&(_, &j)| j == i)
                .map(|(k, _)| k.clone())
                .collect()
        };
        Ok((of(&l.owner), of(&l.externs)))
    }

    /// A provider's configuration is known before it answers anything: a
    /// `provider_config(N, ..)` rule that reads, through any chain of
    /// rules, an extern N answers or an attribute of a type N serves (its
    /// computed values among them) is a compile error naming the cycle.
    /// Reading another provider's is the lazy configuration: its settings
    /// wait on that provider's nulls. `local`: the externs the engine
    /// answers itself (tables, `file.*`, `env_var`).
    pub fn check_configuration(
        &self,
        program: &crate::ast::Program,
        externs: &[crate::ast::ExternFn],
        local: impl Fn(&str) -> bool,
    ) -> Result<()> {
        use crate::ast::Stmt;
        let rules: Vec<(&Atom, &[Lit])> = program
            .statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Rule(r) => Some((&r.head, r.body.as_slice())),
                Stmt::Fact(a) => Some((a, &[][..])),
                _ => None,
            })
            .collect();
        let declared: BTreeSet<&str> = externs.iter().map(|f| f.name.as_str()).collect();
        let mut diags = Vec::new();
        for (head, body) in rules.iter().filter(|(h, _)| h.pred == "provider_config") {
            let Some(name) = head.args.first().and_then(text) else {
                continue;
            };
            let Some(i) = self.link_for(&name) else {
                continue;
            };
            let (types, answers) = self.served(&name, i)?;
            let serving = Serving {
                rules: &rules,
                declared: &declared,
                local: &local,
                types,
                answers,
            };
            if let Some((path, what)) = serving.first_read(head, body) {
                diags.push(cycle(&name, head.span, &path, &what));
            }
        }
        if diags.is_empty() {
            Ok(())
        } else {
            Err(crate::diag::Diagnostics(diags).into())
        }
    }

    /// Every resource's type is declared by the schema of the provider that
    /// will apply it: a type no provider of the stack declares would go to
    /// the fallback, which knows nothing of it (no computed attribute, no
    /// id it mints), so it is a plan error before anything is planned; a
    /// type the program declares itself (a `type` block) is the mock's. The
    /// error names the resource, the stack's providers' `use`s, and the
    /// known schemas that do declare the type. `specs`: the providers this
    /// run started, as `--provider` or the providers' `use`s give them.
    pub fn check_types(&self, program: &crate::ast::Program, specs: &[String]) -> Result<()> {
        use crate::ast::Stmt;
        let owner = &self.loaded().owner;
        let blocks: Vec<&crate::ast::Config> = program
            .statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Provider(c) if self.blocks.contains_key(&c.name) => Some(c),
                _ => None,
            })
            .collect();
        let names: Vec<String> = if !blocks.is_empty() {
            blocks.iter().map(|c| c.name.clone()).collect()
        } else if specs.is_empty() {
            self.names.clone()
        } else {
            specs.to_vec()
        };
        let own = own_types(program);
        // The program's resources and those of each module it uses and
        // each component it makes a resource of (R-113), once each.
        let diags: Vec<crate::diag::Diagnostic> = crate::modules::reached(program)
            .into_iter()
            .filter_map(|s| match s {
                Stmt::Resource(r) => match &r.typ {
                    Term::Val(Value::Str(typ)) => Some((r, typ)),
                    _ => None,
                },
                _ => None,
            })
            // A kind of a provider the program configures is known once
            // the provider is (R-110): the plan lists it under `later`.
            // Nor is a kind its namespace's cluster may serve once a CRD
            // is made, however the provider is configured (R-126): the
            // plan waits on the CRD the program makes, or names the one it
            // lacks.
            .filter(|(_, typ)| {
                !(owner.contains_key(*typ)
                    || own.contains(typ.as_str())
                    || self.waits(typ).is_some()
                    || self.reached(typ).is_some())
            })
            .map(|(r, typ)| undeclared(r.span, typ, &blocks, &names))
            .collect();
        if diags.is_empty() {
            Ok(())
        } else {
            Err(crate::diag::Diagnostics(diags).into())
        }
    }

    /// `expect_account` (`provider_expect_account(Name, Account)` in
    /// `facts`): each configured provider it names reports that account at
    /// Configure, or the run is refused before anything is planned. A
    /// provider still waiting on the program's settings is checked once
    /// they arrive. `secret`: the providers whose expected account a
    /// secret reaches (`secrets::secret_expected_accounts`), named by its
    /// label, `provider/NAME#expect_account`, never its value. `settings`:
    /// the settings a secret reaches, by provider
    /// (`secrets::secret_settings`); an account a provider reports that is
    /// one of their values (a provider that echoes its token) is named by
    /// the setting's label, `provider/NAME#KEY`, never its value.
    pub fn check_accounts<'a>(
        &self,
        facts: impl IntoIterator<Item = &'a Atom> + Clone,
        secret: &BTreeSet<String>,
        settings: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<()> {
        // The values of the secret settings, by their label.
        let mut secrets: Vec<(String, String)> = Vec::new();
        for a in facts
            .clone()
            .into_iter()
            .filter(|a| a.pred == "provider_config")
        {
            let [Term::Val(Value::Str(name)), Term::Val(Value::Obj(given))] = a.args.as_slice()
            else {
                continue;
            };
            for k in settings.get(name).into_iter().flatten() {
                if let Some(v) = given.get(k) {
                    let label = crate::value::null_label("provider", name, k);
                    secrets.push((spell::bare(v), label));
                }
            }
        }
        let reported = |i: usize, got: &String| {
            if let Some(label) = self.secret_accounts.borrow().get(&i) {
                return format!("{} (a secret)", crate::address::label(label));
            }
            match secrets.iter().find(|(v, _)| v == got) {
                Some((_, label)) => format!("{label} (a secret)"),
                None => got.clone(),
            }
        };
        let mut wrong = Vec::new();
        for a in facts.into_iter().filter(|a| a.pred == EXPECT_ACCOUNT) {
            let [Term::Val(Value::Str(name)), Term::Val(want)] = a.args.as_slice() else {
                continue;
            };
            let Some(i) = self.link_for(name) else {
                continue;
            };
            if self.awaiting.borrow().contains(&i) {
                continue;
            }
            let want = spell::bare(want);
            let shown = if secret.contains(name) {
                format!(
                    "{} (a secret)",
                    crate::value::null_label("provider", name, "expect_account")
                )
            } else {
                want.clone()
            };
            match self.accounts.borrow().get(&i) {
                Some(got) if *got == want => {}
                Some(got) => wrong.push(format!(
                    "provider {name} reports account {}, but the program expects {shown} \
                     (expect_account)",
                    reported(i, got)
                )),
                None => wrong.push(format!(
                    "provider {name} reports no account, but the program expects {shown} \
                     (expect_account): {} cannot tell which account its credentials reach",
                    self.names[i]
                )),
            }
        }
        if wrong.is_empty() {
            return Ok(());
        }
        bail!("refusing to plan: {}", wrong.join("; "))
    }
}

/// A term's text, when it is a string.
fn text(t: &Term) -> Option<String> {
    match t {
        Term::Val(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// What a provider serves, for the search of its configuration's rules
/// for a read of it ([`Providers::check_configuration`]): the program's
/// `rules`, the externs it `declared` and those the engine answers
/// itself (`local`), and the provider's `types` and the externs it
/// `answers`.
struct Serving<'p, F> {
    rules: &'p [(&'p Atom, &'p [Lit])],
    declared: &'p BTreeSet<&'p str>,
    local: &'p F,
    types: BTreeSet<String>,
    answers: BTreeSet<String>,
}

impl<F: Fn(&str) -> bool> Serving<'_, F> {
    /// What the body atom `a` reads that the provider serves, if it does.
    fn read_by(&self, a: &Atom) -> Option<String> {
        if self.declared.contains(a.pred.name()) && !(self.local)(a.pred.name()) {
            return self
                .answers
                .contains(a.pred.name())
                .then(|| format!("extern {}", a.pred));
        }
        let typed = matches!(
            a.pred.name(),
            "attr" | "cloud_attr" | "cloud_computed" | "cloud_exists"
        );
        let t = a.args.first().and_then(text).filter(|_| typed)?;
        let what = match (a.args.get(1).and_then(text), a.args.get(2).and_then(text)) {
            (Some(n), Some(p)) if a.pred != "cloud_exists" => Address {
                typ: t.clone(),
                name: n,
            }
            .attr(p.trim_start_matches('.')),
            (Some(n), _) => Address {
                typ: t.clone(),
                name: n,
            }
            .to_string(),
            _ => t.clone(),
        };
        self.types.contains(&t).then_some(what)
    }

    /// Depth first from the rule `head :- body`: the chain of predicates
    /// to the first thing it reads that the provider serves, and that.
    fn first_read(&self, head: &Atom, body: &[Lit]) -> Option<(Vec<String>, String)> {
        let mut seen = BTreeSet::new();
        let mut cells = BTreeSet::new();
        let mut stack: Vec<(&Atom, &[Lit], Vec<String>)> = vec![(head, body, Vec::new())];
        while let Some((h, b, path)) = stack.pop() {
            if let Some(what) = h.args.iter().find_map(|t| reference(t, &self.types)) {
                return Some((path, what));
            }
            for l in b {
                let (Lit::Pos(a) | Lit::Not(a)) = l else {
                    continue;
                };
                if let Some(what) = self.read_by(a) {
                    return Some((path, what));
                }
                // A cell that is not a resource's (a `let`, an input):
                // what contributes to it.
                if a.pred == "attr"
                    && let [t, scope, k, _] = a.args.as_slice()
                    && text(t).is_some_and(|t| crate::transform::is_pseudo_type(&t))
                    && cells.insert((t, scope, k))
                {
                    // The cell is named by the reader that reads it.
                    for (h2, b2) in self.rules {
                        if h2.pred == "arg"
                            && h2.args.len() == 5
                            && (&h2.args[0], &h2.args[1], &h2.args[2]) == (t, scope, k)
                        {
                            stack.push((h2, b2, path.clone()));
                        }
                    }
                    continue;
                }
                if seen.insert(a.pred.name()) {
                    let mut p = path.clone();
                    p.push(a.pred.to_string());
                    for (h2, b2) in self.rules {
                        if h2.pred == a.pred {
                            stack.push((h2, b2, p.clone()));
                        }
                    }
                }
            }
        }
        None
    }
}

/// A reference in a head: `ref(T, N, P)` of one of `types`.
fn reference(t: &Term, types: &BTreeSet<String>) -> Option<String> {
    match t {
        Term::Val(Value::Ref { typ, name, attr }) if types.contains(typ) => Some(
            Address {
                typ: typ.clone(),
                name: name.clone(),
            }
            .attr(attr),
        ),
        Term::Func { name, args } if name == crate::address::REF => match args.as_slice() {
            [Term::Val(Value::Str(t)), Term::Val(Value::Str(n)), p] if types.contains(t) => {
                let a = Address {
                    typ: t.clone(),
                    name: n.clone(),
                };
                Some(match p {
                    Term::Val(Value::Str(p)) => a.attr(p.trim_start_matches('.')),
                    _ => format!("{a}.."),
                })
            }
            _ => None,
        },
        Term::Func { args, .. } | Term::List(args) => args.iter().find_map(|a| reference(a, types)),
        Term::Obj(m) => m.values().find_map(|a| reference(a, types)),
        _ => None,
    }
}

/// The error of provider `name`'s configuration (its rule at `span`)
/// reading `what`, which it serves, through the predicates `path`.
fn cycle(
    name: &str,
    span: crate::ast::Span,
    path: &[String],
    what: &str,
) -> crate::diag::Diagnostic {
    let mut cycle = vec![format!("provider {name}'s configuration")];
    cycle.extend(path.iter().map(|p| format!("reads {p}")));
    cycle.push(format!("reads {what}"));
    cycle.push(format!("which provider {name} serves"));
    crate::diag::Diagnostic::error(
        span,
        format!("provider {name} is configured from {what}, which it serves itself: a cycle"),
    )
    .with_note(format!("the cycle: {}", cycle.join(" -> ")))
    .with_help(
        "a provider's configuration is evaluated before anything it serves: read \
         inputs, settings, tables, env_var, or another provider's values",
    )
}

/// The types `program` declares itself (a `type` block, a `type_*` row),
/// in any module: the mock plays them with the program's shape.
fn own_types(program: &crate::ast::Program) -> BTreeSet<&str> {
    use crate::ast::Stmt;
    crate::modules::nested(&program.statements)
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Pending(p) => {
                let crate::ast::PendingKind::TypeDecl { name, .. } = &p.kind;
                Some(name.as_str())
            }
            Stmt::Fact(a) if a.pred.name().starts_with("type_") => match a.args.first() {
                Some(Term::Val(Value::Str(t))) => Some(t.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The error of a resource at `span` of the type `typ`, which none of the
/// providers `names` (configured by `blocks`, when the program has them)
/// declares: the known schemas that do, and the `use` that adds one.
fn undeclared(
    span: crate::ast::Span,
    typ: &str,
    blocks: &[&crate::ast::Config],
    names: &[String],
) -> crate::diag::Diagnostic {
    let by = crate::schema::declaring(typ);
    // A relationship that is an attribute of one side (R-158).
    if let Some((t, p)) = crate::schema::instead(typ) {
        return crate::diag::Diagnostic::error(
            span,
            format!("{typ} is no resource: it is {t}'s attribute {p}"),
        )
        .with_help(format!(
            "write `{p} = [..]` in the {t}'s block, or add to it from \
             anywhere with `set R.{p} = [..] where ..`: each write adds its \
             elements"
        ));
    }
    let who = match names {
        [one] => format!("provider {one} does not declare"),
        many => format!("none of the providers {} declares", many.join(", ")),
    };
    let known = match by.as_slice() {
        [] => "no known provider schema declares it".to_string(),
        by => format!("declared by: {}", by.join(", ")),
    };
    let mut d = crate::diag::Diagnostic::error(span, format!("{who} {typ}; {known}"));
    for c in blocks {
        d = d.with_label(c.span, format!("provider {}", c.name));
    }
    // A provider's types are named under it (R-36): `aws.vpc` is
    // provider aws's. The fake cloud's namespaces (`net`, `iam`) name no
    // provider, so where a known schema gives the type to another
    // provider, that schema is the one to configure.
    let ns = typ.split_once('.').map(|(ns, _)| ns).filter(|ns| {
        by.is_empty()
            || by.iter().any(|n| {
                crate::schema::load_provider(n)
                    .is_ok_and(|s| s.provider_of.get(typ).is_some_and(|p| p == ns))
            })
    });
    match (ns, by.first()) {
        (Some(ns), _) if !names.iter().any(|n| n == ns) => d.with_help(format!(
            "{typ} is provider {ns}'s type: add `use {ns}` to the stack"
        )),
        (None, Some(first)) => {
            d.with_help(format!("configure the provider that does: `use {first}`"))
        }
        _ => d,
    }
}
