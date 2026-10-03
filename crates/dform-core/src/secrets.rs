//! Static secret labels (E DR-19, A's information-flow pass): one dataflow
//! fixpoint over predicate signatures, `public < secret`.
//!
//! Sources: a schema attribute marked `sensitive` (an `attr` read of it, a
//! `ref` to it), an input declared `secret(T)`, an extern column declared
//! `-v: secret(T)`, a function declared `-> secret(T)` (`random.password`),
//! another stack's output published as secret (the value
//! of a `stack_output` of it: `stack::Published`). A `memo.first` keeps a
//! secret when its candidate is one: that literal's value is secret, and
//! not another's (`secret_memos`). A head position is secret when a secret value reaches
//! it through its rule: a variable bound at a secret position, or built
//! from one (`format`, arithmetic, lists, objects, field access).
//!
//! Then each rule is checked, each violation a compile error with a span:
//!
//! - E0301 a comparison, a builtin predicate or an inspecting function
//!   (`len`, `split`, `inet_*`, ...) over a secret: comparing leaks a bit;
//! - E0302 a negated literal over a secret: absence leaks a bit;
//! - E0303 an aggregate other than `collect_*` over a secret: `count`
//!   leaks cardinality;
//! - E0304 a secret reaching a public place: a resource attribute the
//!   schema does not mark `sensitive`, a setting, an output not declared
//!   `secret(T)`, an input not declared `secret(T)`, a `deny`/`warn`;
//! - E0305 a secret reaching a resource address (`want`, `arg`, `ref`,
//!   `scoped`): names are printed everywhere.
//!
//! `declassify(V, Reason)` is the one way out: its value is public, and
//! what is inside it may be inspected (`declassify(len(Pw), "...")`). The
//! lowering derives `declassified(Site, Reason)` for policy to read.
//!
//! An input's own refinement (`input pw: secret(string) where ...`) is the
//! boundary where a secret may be checked, so its generated rules are
//! exempt from E0301/E0302.

use crate::ast::{Atom, Lit, Program, Span, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::schema::Schema;
use crate::transform::Lowered;
use crate::value::Value;
use anyhow::Result;
use std::collections::BTreeSet;

/// Whether a secret flows through `name` uninspected: a function declared
/// `forwards` (`std/*.df`), or an aggregate that only collects.
fn carries(name: &str) -> bool {
    COLLECT.contains(&name) || crate::functions::get(name).is_some_and(|f| f.forwards)
}

/// `declassify(V, Reason)`: `V`, public (`transform` derives
/// `declassified/2` beside the rule for policy).
const DECLASSIFY: &str = "declassify";

/// Aggregates that only collect: their result is secret, nothing leaks.
const COLLECT: &[&str] = &["collect_set", "collect_list"];

fn s(t: &Term) -> Option<&str> {
    match t {
        Term::Val(Value::Str(x)) => Some(x),
        _ => None,
    }
}

struct Pass<'a> {
    schema: &'a Schema,
    /// Secret positions: (predicate, column).
    secret: BTreeSet<(String, usize)>,
    /// Pseudo-type cells declared secret: (type, scope, key).
    cells: BTreeSet<(String, String, String)>,
    /// Other stacks' secret outputs: (deployment, key).
    outputs: &'a BTreeSet<(String, String)>,
}

impl Pass<'_> {
    /// Is `attr(T, A, P, _)` a secret cell?
    fn attr_secret(&self, typ: &Term, addr: &Term, path: &Term) -> bool {
        match (s(typ), s(path)) {
            (Some(t), Some(p)) => {
                let p = p.trim_start_matches('.');
                if crate::transform::is_pseudo_type(t) {
                    return s(addr).is_some_and(|a| {
                        self.cells
                            .contains(&(t.to_string(), a.to_string(), p.to_string()))
                    });
                }
                // A read of an object holding a sensitive leaf is secret too.
                self.schema.is_sensitive(t, p)
                    || self.schema.facts.iter().any(|f| {
                        f.pred == "type_attr"
                            && s(&f.args[0]) == Some(t)
                            && s(&f.args[1]).is_some_and(|q| q.starts_with(&format!("{p}.")))
                            && self.flag(f, "sensitive")
                    })
            }
            // A path the program computes: secret if any it could be is.
            (Some(t), None) => self.schema.facts.iter().any(|f| {
                f.pred == "type_attr" && s(&f.args[0]) == Some(t) && self.flag(f, "sensitive")
            }),
            (None, _) => self
                .schema
                .facts
                .iter()
                .any(|f| f.pred == "type_attr" && self.flag(f, "sensitive")),
        }
    }

    fn flag(&self, f: &Atom, flag: &str) -> bool {
        matches!(f.args.get(3), Some(Term::Val(Value::List(fs))) if fs.contains(&Value::Str(flag.into())))
            || matches!(f.args.get(3), Some(Term::List(fs)) if fs.iter().any(|t| s(t) == Some(flag)))
    }

    /// Is `t` secret given the secret variables `vars`?
    fn term_secret(&self, t: &Term, vars: &BTreeSet<String>) -> bool {
        match t {
            Term::Var(v) => vars.contains(v),
            // Its label lowered to public.
            Term::Func { name, .. } if name == DECLASSIFY => false,
            // A function whose value is a secret (`-> secret(T)`).
            Term::Func { name, .. } if returns_secret(name) => true,
            Term::Func { name, args } if name == "ref" && args.len() == 3 => {
                self.attr_secret(&args[0], &args[1], &args[2])
                    || args.iter().any(|a| self.term_secret(a, vars))
            }
            Term::Func { args, .. } | Term::List(args) => {
                args.iter().any(|a| self.term_secret(a, vars))
            }
            Term::Obj(m) => m.values().any(|a| self.term_secret(a, vars)),
            _ => false,
        }
    }

    /// The secret variables of a body, to a fixpoint (an equality may come
    /// before what binds its other side).
    fn body_vars(&self, body: &[Lit]) -> BTreeSet<String> {
        let mut vars = BTreeSet::new();
        loop {
            let before = vars.len();
            for l in body {
                match l {
                    // A memo's value is as secret as its candidate.
                    Lit::Pos(a) if a.pred == crate::memo::FIRST && a.args.len() == 3 => {
                        if self.term_secret(&a.args[1], &vars) {
                            collect_vars(&a.args[2], &mut vars);
                        }
                    }
                    Lit::Pos(a) => {
                        for (i, t) in a.args.iter().enumerate() {
                            if self.position_secret(a, i) || self.term_secret(t, &vars) {
                                collect_vars(t, &mut vars);
                            }
                        }
                    }
                    Lit::Eq(x, y) => {
                        if self.term_secret(x, &vars) {
                            collect_vars(y, &mut vars);
                        }
                        if self.term_secret(y, &vars) {
                            collect_vars(x, &mut vars);
                        }
                    }
                    _ => {}
                }
            }
            if vars.len() == before {
                return vars;
            }
        }
    }

    /// The first leaf of a contribution `arg(T, A, P, V)` where a secret
    /// meets a public path: `V` is taken apart by object keys (a
    /// contribution to `a.b` is `{b: V}` at `a`).
    fn public_leaf(
        &self,
        typ: &Term,
        addr: &Term,
        path: &Term,
        value: &Term,
        vars: &BTreeSet<String>,
    ) -> Option<String> {
        if !self.term_secret(value, vars) {
            return None;
        }
        // Is the path public: not sensitive, nor under a sensitive one?
        let here = |p: &str| match s(typ) {
            Some(t) if !crate::transform::is_pseudo_type(t) => !self.schema.is_sensitive(t, p),
            _ => !self.attr_secret(typ, addr, &Term::Val(Value::Str(p.into()))),
        };
        let Some(p) = s(path) else {
            return (!self.attr_secret(typ, addr, path)).then(|| "?".to_string());
        };
        let p = p.trim_start_matches('.').to_string();
        if !here(&p) {
            return None;
        }
        match value {
            Term::Obj(m) => m.iter().find_map(|(k, v)| {
                self.public_leaf(
                    typ,
                    addr,
                    &Term::Val(Value::Str(format!("{p}.{k}"))),
                    v,
                    vars,
                )
            }),
            _ => Some(p),
        }
    }

    fn position_secret(&self, a: &Atom, i: usize) -> bool {
        match (a.pred.as_str(), a.args.len()) {
            ("attr" | "world_attr", 4) if i == 3 => {
                self.attr_secret(&a.args[0], &a.args[1], &a.args[2])
            }
            // A name or key the program computes: secret if any it could
            // be is.
            ("stack_output", 3) if i == 2 => {
                let (d, k) = (s(&a.args[0]), s(&a.args[1]));
                self.outputs
                    .iter()
                    .any(|(x, y)| d.is_none_or(|d| d == x) && k.is_none_or(|k| k == y))
            }
            _ => self.secret.contains(&(a.pred.clone(), i)),
        }
    }
}

fn collect_vars(t: &Term, out: &mut BTreeSet<String>) {
    match t {
        Term::Var(v) => {
            out.insert(v.clone());
        }
        Term::Func { args, .. } | Term::List(args) => {
            args.iter().for_each(|a| collect_vars(a, out))
        }
        Term::Obj(m) => m.values().for_each(|a| collect_vars(a, out)),
        _ => {}
    }
}

/// Every rule and constraint: (head, body, span, is an input refinement).
fn rules(program: &Program) -> Vec<(Option<&Atom>, &[Lit], Span)> {
    program
        .statements
        .iter()
        .filter_map(|st| match st {
            Stmt::Rule(r) => Some((Some(&r.head), r.body.as_slice(), r.head.span)),
            Stmt::Fact(a) => Some((Some(a), &[][..], a.span)),
            _ => None,
        })
        .collect()
}

fn is_refinement(head: Option<&Atom>) -> bool {
    head.is_some_and(|h| {
        h.pred.rsplit("::").next().unwrap_or(&h.pred).starts_with("__refine_")
            || (h.pred == "deny"
                && matches!(h.args.first(), Some(Term::Val(Value::Str(m))) if m.contains("fails its refinement")))
    })
}

/// Whether `name` is a function declared `-> secret(T)`.
fn returns_secret(name: &str) -> bool {
    crate::functions::get(name).is_some_and(|f| f.ret.starts_with("secret("))
}

fn is_secret_ty(t: &TypeExpr) -> bool {
    matches!(t, TypeExpr::Apply(n, _) if n == "secret")
}

/// The secret positions of a lowered program: the fixpoint over predicate
/// signatures, from the sources to every head a secret reaches.
fn fixpoint<'a>(
    lowered: &Lowered,
    schema: &'a Schema,
    outputs: &'a BTreeSet<(String, String)>,
) -> Pass<'a> {
    let mut pass = Pass {
        schema,
        secret: BTreeSet::new(),
        cells: BTreeSet::new(),
        outputs,
    };
    for d in &lowered.inputs {
        if is_secret_ty(&d.decl.ty) {
            pass.cells.insert((
                crate::modules::INPUT.into(),
                d.scope.clone(),
                d.decl.name.clone(),
            ));
        }
    }
    for (scope, k) in &lowered.secret_outputs {
        pass.cells
            .insert((crate::transform::OUTPUT.into(), scope.clone(), k.clone()));
    }
    for f in &lowered.extern_fns {
        for (i, b) in f.args.iter().enumerate() {
            if crate::externs::is_secret(b) {
                pass.secret.insert((f.name.clone(), i));
            }
        }
    }
    let rs = rules(&lowered.program);
    // The fixpoint over predicate signatures.
    loop {
        let before = (pass.secret.len(), pass.cells.len());
        for (head, body, _) in &rs {
            let Some(h) = head else { continue };
            let vars = pass.body_vars(body);
            for (i, t) in h.args.iter().enumerate() {
                if pass.term_secret(t, &vars) {
                    pass.secret.insert((h.pred.clone(), i));
                }
            }
            // A `let` holding a secret is a secret cell (R-3).
            if let ("arg", [t, scope, k, v, _]) = (h.pred.as_str(), h.args.as_slice())
                && s(t) == Some(crate::modules::LET)
                && pass.term_secret(v, &vars)
                && let (Some(scope), Some(k)) = (s(scope), s(k))
            {
                pass.cells.insert((
                    crate::modules::LET.to_string(),
                    scope.to_string(),
                    k.to_string(),
                ));
            }
        }
        if (pass.secret.len(), pass.cells.len()) == before {
            break;
        }
    }
    pass
}

/// The providers whose `expect_account` a secret reaches (an `env_var`, a
/// secret input): a refusal names that account by its label, never its
/// value (`Providers::check_accounts`).
/// `outputs`: other stacks' secret outputs the run read, (deployment, key).
pub fn secret_expected_accounts(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> BTreeSet<String> {
    let pass = fixpoint(lowered, schema, outputs);
    rules(&lowered.program)
        .into_iter()
        .filter_map(|(head, body, _)| {
            let h = head.filter(|h| h.pred == crate::plugin::providers::EXPECT_ACCOUNT)?;
            let vars = pass.body_vars(body);
            match h.args.as_slice() {
                [name, account] if pass.term_secret(account, &vars) => s(name).map(str::to_string),
                _ => None,
            }
        })
        .collect()
}

/// The `memo.first` literals whose candidate is a secret: their value is
/// kept sealed (`memo`), and the plan file records none of their calls.
pub fn secret_memos(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> Vec<Atom> {
    let pass = fixpoint(lowered, schema, outputs);
    let mut out = Vec::new();
    for (_, body, _) in rules(&lowered.program) {
        let vars = pass.body_vars(body);
        for l in body {
            if let Lit::Pos(a) = l
                && a.pred == crate::memo::FIRST
                && a.args.len() == 3
                && pass.term_secret(&a.args[1], &vars)
            {
                out.push(a.clone());
            }
        }
    }
    out
}

/// The pass over a lowered program against the provider schema; `outputs`
/// are other stacks' secret outputs the run read, (deployment, key).
pub fn check(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> Result<()> {
    let pass = fixpoint(lowered, schema, outputs);
    let rs = rules(&lowered.program);

    let mut diags = Vec::new();
    for (head, body, span) in &rs {
        let vars = pass.body_vars(body);
        let refinement = is_refinement(*head);
        let secret = |t: &Term| pass.term_secret(t, &vars);
        for l in body.iter() {
            match l {
                Lit::Neq(x, y) | Lit::Gt(x, y) | Lit::Ge(x, y) | Lit::Lt(x, y) | Lit::Le(x, y)
                    if !refinement && (secret(x) || secret(y)) =>
                {
                    diags.push(e0301(*span, "a comparison"));
                }
                Lit::Eq(x, y) if !refinement => {
                    // `X = f(Secret)`: a function that inspects it.
                    for t in [x, y] {
                        if let Some(f) = inspecting(t, &|t| secret(t)) {
                            diags.push(e0301(*span, &format!("{f}()")));
                        }
                    }
                    // A test between two terms, neither a fresh variable.
                    if !matches!(x, Term::Var(_))
                        && !matches!(y, Term::Var(_))
                        && (secret(x) || secret(y))
                    {
                        diags.push(e0301(*span, "an equality test"));
                    }
                }
                Lit::Pos(a) if !refinement && is_builtin_pred(&a.pred) => {
                    if a.args.iter().any(&secret) {
                        diags.push(e0301(a.span, &format!("{}/{}", a.pred, a.args.len())));
                    }
                }
                Lit::Not(a) if !refinement => {
                    let bound_secret = a.args.iter().enumerate().any(|(i, t)| {
                        secret(t) || (pass.position_secret(a, i) && !matches!(t, Term::Wildcard))
                    });
                    if bound_secret {
                        diags.push(Diagnostic::error(
                            a.span,
                            format!(
                                "E0302: `not {}(...)` over a secret: its absence leaks a bit",
                                a.pred
                            ),
                        ));
                    }
                }
                _ => {}
            }
        }
        let Some(h) = head else { continue };
        // E0303: an aggregate that is not a collect.
        for t in &h.args {
            if let Term::Func { name, args } = t
                && matches!(
                    name.as_str(),
                    "count" | "sum" | "min" | "max" | "any" | "all"
                )
                && args.iter().any(&secret)
            {
                diags.push(Diagnostic::error(
                    h.span,
                    format!("E0303: {name}() over a secret leaks it; only collect_* may aggregate a secret"),
                ));
            }
            if let Some(f) = inspecting(t, &|t| secret(t))
                && !COLLECT.contains(&f.as_str())
            {
                diags.push(e0301(h.span, &format!("{f}()")));
            }
        }
        // E0305: a name.
        let named = |t: &Term| secret(t) || names_secret(t, &|t| secret(t));
        let addr = crate::zset::address_arg(h);
        if addr.is_some_and(named) || h.args.iter().any(|t| names_secret(t, &|t| secret(t))) {
            diags.push(Diagnostic::error(
                h.span,
                "E0305: a secret reaches a resource address; names are printed everywhere",
            ));
        }
        // E0304: a public place.
        match (h.pred.as_str(), h.args.len()) {
            ("arg", 5)
                if let Some(leak) =
                    pass.public_leaf(&h.args[0], &h.args[1], &h.args[2], &h.args[3], &vars) =>
            {
                let place = match (s(&h.args[0]), Some(leak.as_str())) {
                    (Some(crate::transform::SETTINGS), Some(p)) => format!("setting .{p}"),
                    (Some(crate::transform::OUTPUT), Some(p)) => {
                        format!("output {p}, not declared secret(T)")
                    }
                    (Some(crate::modules::INPUT), Some(p)) => {
                        format!("input {p}, not declared secret(T)")
                    }
                    (Some(t), Some("?")) => format!("{t} at a path the program computes"),
                    (Some(t), Some(p)) => format!("{t} .{p}, not marked sensitive in the schema"),
                    _ => "an attribute path the program computes".to_string(),
                };
                diags.push(Diagnostic::error(
                    h.span,
                    format!("E0304: a secret reaches {place}"),
                ));
            }
            ("deny" | "warn", _) if !refinement && h.args.iter().any(secret) => {
                diags.push(Diagnostic::error(
                    h.span,
                    format!(
                        "E0304: a secret reaches a {} message or context, which is printed",
                        h.pred
                    ),
                ));
            }
            _ => {}
        }
    }
    if diags.is_empty() {
        Ok(())
    } else {
        diags.dedup_by(|a, b| a.render(false) == b.render(false));
        Err(Diagnostics(diags).into())
    }
}

fn e0301(span: Span, what: &str) -> Diagnostic {
    Diagnostic::error(
        span,
        format!("E0301: {what} over a secret: inspecting a secret leaks it"),
    )
    .with_help("check it at the input: `input k: secret(T) where ...`, or leave it to the provider")
}

fn is_builtin_pred(p: &str) -> bool {
    matches!(p, "member" | "enumerate") || crate::functions::is_predicate(p)
}

/// The first function in `t` that inspects a secret argument.
fn inspecting(t: &Term, secret: &dyn Fn(&Term) -> bool) -> Option<String> {
    match t {
        // What is declassified may be inspected: the rule says so.
        Term::Func { name, .. } if name == DECLASSIFY => None,
        Term::Func { name, args } => {
            if !carries(name) && args.iter().any(secret) {
                return Some(name.clone());
            }
            args.iter().find_map(|a| inspecting(a, secret))
        }
        Term::List(xs) => xs.iter().find_map(|a| inspecting(a, secret)),
        Term::Obj(m) => m.values().find_map(|a| inspecting(a, secret)),
        _ => None,
    }
}

/// Does `t` name a resource with a secret (`ref(T, Secret, P)`,
/// `scoped(S, Secret)`)?
fn names_secret(t: &Term, secret: &dyn Fn(&Term) -> bool) -> bool {
    match t {
        Term::Func { name, .. } if name == DECLASSIFY => false,
        Term::Func { name, args } => {
            (name == "ref" && args.len() == 3 && secret(&args[1]))
                || (name == "scoped" && args.iter().any(secret))
                || args.iter().any(|a| names_secret(a, secret))
        }
        Term::List(xs) => xs.iter().any(|a| names_secret(a, secret)),
        Term::Obj(m) => m.values().any(|a| names_secret(a, secret)),
        _ => false,
    }
}

// The held store (R-60): what dform itself keeps of a secret. A
// `memo.first` of a secret candidate is sealed with a key derived from the
// stack's key (`state.key`) before it goes into state, and opened in
// memory by the run that reads it; `random.*` derives from a master
// secret by HKDF. HMAC-SHA256 is the one primitive: the seal is
// encrypt-then-MAC with HMAC in counter mode as the stream (a PRF keyed
// apart from the MAC key), so no cipher crate is needed.

/// HMAC-SHA256 (RFC 2104) of the concatenation of `parts` under `key`.
pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new().chain_update(k.map(|x| x ^ 0x36));
    for p in parts {
        inner.update(p);
    }
    Sha256::new()
        .chain_update(k.map(|x| x ^ 0x5c))
        .chain_update(inner.finalize())
        .finalize()
        .into()
}

/// HKDF-SHA256 (RFC 5869): `len` bytes (at most 255 blocks) of key
/// material for `info` from the input key material `ikm`.
pub fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let prk = hmac(salt, &[ikm]);
    let mut out = Vec::with_capacity(len);
    let mut t: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < len {
        t = hmac(&prk, &[&t, info, &[i]]).to_vec();
        out.extend_from_slice(&t);
        i = i.checked_add(1).expect("hkdf: at most 255 blocks");
    }
    out.truncate(len);
    out
}

/// A key's 32 bytes, from its hex.
fn key_bytes(k: &crate::zset::file::Key) -> [u8; 32] {
    let hex = k.to_hex();
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("a key is hex");
    }
    out
}

/// The bytes the stack's key derives for `what` (`Key::derive`): a key
/// that says nothing of the stack's, for one use.
pub fn derived(k: &crate::zset::file::Key, what: &str) -> [u8; 32] {
    key_bytes(&k.derive(what))
}

/// `bytes` XOR the HMAC-CTR stream of `key` and `nonce`.
fn stream(key: &[u8], nonce: &[u8], bytes: &[u8]) -> Vec<u8> {
    bytes
        .chunks(32)
        .enumerate()
        .flat_map(|(i, c)| {
            let ks = hmac(key, &[nonce, &(i as u64).to_be_bytes()]);
            c.iter().zip(ks).map(|(b, k)| b ^ k).collect::<Vec<_>>()
        })
        .collect()
}

/// `plain` sealed under the stack key `k` for `label` (what it is bound
/// to: another label's seal does not open as this one): base64 of a
/// random nonce, the ciphertext and its tag.
pub fn seal(k: &crate::zset::file::Key, label: &str, plain: &[u8]) -> Result<String> {
    use base64::Engine;
    use std::io::Read;
    let mut nonce = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut nonce))
        .map_err(|e| anyhow::anyhow!("read /dev/urandom for a seal's nonce: {e}"))?;
    let ct = stream(&derived(k, "held store: stream"), &nonce, plain);
    let tag = hmac(
        &derived(k, "held store: mac"),
        &[label.as_bytes(), &[0], &nonce, &ct],
    );
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    out.extend_from_slice(&tag);
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

/// What [`seal`] sealed for `label` under `k`; an error when the seal is
/// not one (another key, another label, altered).
pub fn open(k: &crate::zset::file::Key, label: &str, sealed: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .map_err(|e| anyhow::anyhow!("{label}: the held value is not base64: {e}"))?;
    if bytes.len() < 48 {
        anyhow::bail!("{label}: the held value is too short to be a seal");
    }
    let (nonce, rest) = bytes.split_at(16);
    let (ct, tag) = rest.split_at(rest.len() - 32);
    let want = hmac(
        &derived(k, "held store: mac"),
        &[label.as_bytes(), &[0], nonce, ct],
    );
    // Compared in constant time.
    if want.iter().zip(tag).fold(0u8, |d, (a, b)| d | (a ^ b)) != 0 {
        anyhow::bail!(
            "{label}: the held value does not open with this stack's key (state.key): \
             another stack's key, or altered"
        );
    }
    Ok(stream(&derived(k, "held store: stream"), nonce, ct))
}

#[cfg(test)]
mod held_tests {
    use super::*;

    /// RFC 4231 test case 2 and RFC 5869 test case 1.
    #[test]
    fn hmac_and_hkdf_match_their_rfcs() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(
            hex(&hmac(b"Jefe", &[b"what do ya want ", b"for nothing?"])),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0u8..=0x0c).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        assert_eq!(
            hex(&hkdf(&salt, &ikm, &info, 42)),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn a_seal_opens_with_its_key_and_label_only() {
        let k = crate::zset::file::Key::from_hex(&"11".repeat(32)).unwrap();
        let other = crate::zset::file::Key::from_hex(&"22".repeat(32)).unwrap();
        let s = seal(
            &k,
            "db-pw",
            b"hunter2-but-longer-than-one-block-of-32-bytes",
        )
        .unwrap();
        assert!(!s.contains("hunter2"));
        assert_eq!(
            open(&k, "db-pw", &s).unwrap(),
            b"hunter2-but-longer-than-one-block-of-32-bytes"
        );
        assert!(open(&other, "db-pw", &s).is_err());
        assert!(open(&k, "other", &s).is_err());
        assert_ne!(
            s,
            seal(
                &k,
                "db-pw",
                b"hunter2-but-longer-than-one-block-of-32-bytes"
            )
            .unwrap()
        );
    }
}
