//! The function registry (DESIGN.org R-6): every function a term may call,
//! declared in the signature files `std/*.df` shipped with dform and read
//! here into one table that the engine, the resolver, the secrets pass, the
//! language server and the reference share.
//!
//! ```text
//! package inet
//! #| The `n`th subnet of `net` with `bits` more prefix bits.
//! #| example: let cidr = inet.subnet(vpc.cidr, 4, n)
//! fn subnet(net: inet, bits: int, n: int) -> inet?
//! ```
//!
//! A function is named by its package, the type it is about
//! (`inet.subnet`, `str.format`); there is no prelude and no bare function
//! (R-155) but the lowering's own (`add`, `__path`), which no program
//! writes. `?` after
//! the result marks a partial function, `forwards` one a secret flows
//! through uninspected (`secrets`, E0301), `forwards nulls` one whose null
//! arguments are not content positions (Rule 2), and `internal` the
//! lowering's own (`add`, `__path`), which a program may not call. A
//! function is pure, or a coeffect, `reads` (`io.read`): a read the
//! context satisfies, lowered to a table, never a body.
//! The bodies are this module's ([`body`], [`BODIES`]), which the engine
//! looks up by the qualified name; a test keeps the two in step.
//!
//! The format is self-contained text (DESIGN.org R-24: a function package
//! embeds its signature file), read for now by this module rather than by
//! the program parser. When R-24 embeds these files in packages, the
//! program parser takes the format over (a non-program mode) and this
//! reader goes.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::spell;
use crate::value::Value;

/// The signature files, by the path they are shipped at.
pub const SOURCES: &[(&str, &str)] = &[
    ("lowering.df", include_str!("lowering.df")),
    ("std/inet.df", include_str!("../../../std/inet.df")),
    ("std/int.df", include_str!("../../../std/int.df")),
    ("std/ip.df", include_str!("../../../std/ip.df")),
    ("std/str.df", include_str!("../../../std/str.df")),
    ("std/list.df", include_str!("../../../std/list.df")),
    ("std/time.df", include_str!("../../../std/time.df")),
    ("std/random.df", include_str!("../../../std/random.df")),
    ("std/regex.df", include_str!("../../../std/regex.df")),
    ("std/semver.df", include_str!("../../../std/semver.df")),
    ("std/oci.df", include_str!("../../../std/oci.df")),
    ("std/hash.df", include_str!("../../../std/hash.df")),
    ("std/base64.df", include_str!("../../../std/base64.df")),
    ("std/uri.df", include_str!("../../../std/uri.df")),
    ("std/path.df", include_str!("../../../std/path.df")),
    ("std/json.df", include_str!("../../../std/json.df")),
    ("std/yaml.df", include_str!("../../../std/yaml.df")),
    ("std/toml.df", include_str!("../../../std/toml.df")),
    ("std/quantity.df", include_str!("../../../std/quantity.df")),
    ("std/secret.df", include_str!("../../../std/secret.df")),
    ("std/csv.df", include_str!("../../../std/csv.df")),
    ("std/io.df", include_str!("../../../std/io.df")),
];

/// The operators and the types each is over (R-155: polymorphism lives in
/// operators and fields, never in functions), as docs/grammar.md's
/// operator table writes them; `==` is over every type. The std audit
/// checks the table against this and this against what the engine does.
pub const OPERATORS: &[(&str, &[&str])] = &[
    ("in", &["list", "string", "inet", "range"]),
    ("+ -", &["int", "float", "bytes", "cpu", "duration", "time"]),
    ("* /", &["int", "float", "bytes", "cpu", "duration"]),
    ("%", &["int", "float"]),
    (
        "< <= > >=",
        &[
            "int", "float", "bytes", "cpu", "duration", "time", "semver", "ip",
        ],
    ),
    (
        "${..}",
        &[
            "string", "int", "float", "bool", "bytes", "cpu", "duration", "time", "semver", "ip",
            "inet", "range", "uri", "oci",
        ],
    ),
];

/// A function's kind (R-155): pure, or a coeffect, a read the context
/// satisfies. There is no third.
pub fn kind(f: &Function) -> &'static str {
    match f.coeffect {
        true => "coeffect",
        false => "pure",
    }
}

/// The package of the lowering's own functions (crates/dform-core/src/
/// lowering.df): bare, each `internal`, written by no program. Every
/// other function is named by its package (R-155: no prelude).
pub const LOWERING: &str = "lowering";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: String,
    /// `name?: T`: a call may leave it out (the last parameters only).
    pub optional: bool,
    /// `name?: T = LITERAL`: what the parameter is when a call names a
    /// later one and leaves this out (`random.password("db", generation:
    /// 1)`); an int, a bool or a quoted string.
    pub default: Option<String>,
}

impl Param {
    /// The default's value.
    pub fn default_value(&self) -> Option<crate::value::Value> {
        use crate::value::Value;
        let d = self.default.as_deref()?;
        if let Ok(i) = d.parse::<i64>() {
            return Some(Value::Int(i));
        }
        match d {
            "true" => return Some(Value::Bool(true)),
            "false" => return Some(Value::Bool(false)),
            _ => {}
        }
        let s = d.strip_prefix('"')?.strip_suffix('"')?;
        Some(Value::Str(s.to_string()))
    }

    /// As a signature writes it: `length?: int = 32`.
    pub fn text(&self) -> String {
        let q = if self.optional { "?" } else { "" };
        match &self.default {
            Some(d) => format!("{}{q}: {} = {d}", self.name, self.ty),
            None => format!("{}{q}: {}", self.name, self.ty),
        }
    }
}

/// One declared function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// The name a call is written with: `inet.subnet`, or bare for the
    /// lowering's own (`add`, `__path`).
    pub name: String,
    pub package: String,
    pub params: Vec<Param>,
    /// A last parameter `...`: any number more of the one before it.
    pub variadic: bool,
    pub ret: String,
    /// `-> T?`: a call may have no value.
    pub partial: bool,
    /// `forwards`: a secret argument flows to the result uninspected.
    pub forwards: bool,
    /// `forwards nulls`: a null argument is not a content position.
    pub forwards_nulls: bool,
    /// `internal`: the lowering's, not callable from a program.
    pub internal: bool,
    /// `reads`: a coeffect, a read the context satisfies (R-155,
    /// `io.read`): it lowers to a table the host answers, never a body.
    /// Every other function is pure.
    pub coeffect: bool,
    pub summary: String,
    pub example: String,
    /// `inet.subnet(net: inet, bits: int, n: int) -> inet?`
    pub signature: String,
    /// Where it is declared: the shipped path and its line (from 1).
    pub file: String,
    pub line: usize,
}

impl Function {
    /// Whether `n` arguments fit its parameters.
    pub fn takes(&self, n: usize) -> bool {
        let required = self.params.iter().filter(|p| !p.optional).count();
        if self.variadic {
            n >= self.params.len()
        } else {
            (required..=self.params.len()).contains(&n)
        }
    }

    /// A function to bool is also a predicate: `inet.overlaps(a, b)` as a
    /// body literal holds when the call is true.
    pub fn is_predicate(&self) -> bool {
        self.ret == "bool"
    }
}

/// Every declared function, by name.
#[derive(Debug, Default)]
pub struct Registry {
    by_name: BTreeMap<String, Function>,
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    Registry::load(SOURCES).unwrap_or_else(|e| panic!("the std signature files: {e}"))
});

/// The registry of the shipped signature files.
pub fn registry() -> &'static Registry {
    &REGISTRY
}

/// The declared function `name` (internal ones included).
pub fn get(name: &str) -> Option<&'static Function> {
    registry().get(name)
}

/// A call's arguments with its named ones (`random.password("db",
/// generation: 1)`) put where `f` declares them: each names a parameter
/// the positional ones do not give, and a parameter left out before it
/// takes its declared default; an error says which is wrong.
pub fn with_named(
    f: &Function,
    positional: Vec<crate::ast::Term>,
    named: Vec<(String, crate::ast::Term)>,
) -> Result<Vec<crate::ast::Term>, String> {
    use crate::ast::Term;
    let mut at: Vec<Option<Term>> = positional.into_iter().map(Some).collect();
    for (k, t) in named {
        let Some(i) = f.params.iter().position(|p| p.name == k) else {
            let names: Vec<&str> = f.params.iter().map(|p| p.name.as_str()).collect();
            return Err(format!(
                "`{}` has no parameter `{k}`: its parameters are {}",
                f.name,
                names.join(", ")
            ));
        };
        if at.len() <= i {
            at.resize(i + 1, None);
        }
        if at[i].is_some() {
            return Err(format!("`{}`'s `{k}` is given twice", f.name));
        }
        at[i] = Some(t);
    }
    at.into_iter()
        .zip(&f.params)
        .map(|(t, p)| match t {
            Some(t) => Ok(t),
            None => p.default_value().map(Term::Val).ok_or_else(|| {
                format!(
                    "`{}`'s `{}` is left out before a named argument, and has no default: give \
                     it (`{}`)",
                    f.name, p.name, f.signature
                )
            }),
        })
        .collect()
}

/// The lowering's functions a program writes as forms of the language
/// (`ref(r)`, R-43; `cloud_ref(T, name, path)`): callable, but no part of
/// the standard library's listing, `dform doc` or the editor (R-134).
/// `{ "${k}": V }`: an object whose keys are computed, its keys and
/// values in turn (lowering.df).
pub const OBJECT: &str = "__object";

/// `{ ..base, k: v }` (R-199): an object of its parts' fields in turn, a
/// later key replacing an earlier (lowering.df).
pub const MERGE: &str = "__merge";

/// `[..a, x, ..b]` (R-199): a list of its parts' elements in turn, a
/// discrete range's members (lowering.df).
pub const CONCAT: &str = "__concat";

/// `{ k: p, ..rest }` (R-199): an object pattern's rest, the value without
/// the keys the pattern names (lowering.df).
pub const REST: &str = "__rest";

pub const FORMS: &[&str] = &["ref", "cloud_ref"];

/// Whether a program may call `name`: declared and not internal, or one
/// of the [`FORMS`].
pub fn callable(name: &str) -> bool {
    get(name).is_some_and(|f| !f.internal) || FORMS.contains(&name)
}

/// A call of `name` as a message names it: `str.split()`, or the field
/// `x.len` lowers from (R-155).
pub fn shown_call(name: &str) -> String {
    match name {
        crate::address::LEN => "`.len`".to_string(),
        OBJECT => "an object's key".to_string(),
        MERGE | CONCAT => "a spread `..`".to_string(),
        REST => "a pattern's rest `..`".to_string(),
        n => format!("{n}()"),
    }
}

/// Whether `name` is a builtin predicate: a function to bool.
pub fn is_predicate(name: &str) -> bool {
    get(name).is_some_and(Function::is_predicate)
}

/// The error at a call of `name`, which no program may call: unknown, or
/// internal to the lowering. The help names the function meant when one
/// is a qualification away (`split` is `str.split`, `inet_subnet`
/// `inet.subnet`, `to_int` the constructor `int`).
pub fn unknown(span: crate::ast::Span, name: &str) -> crate::diag::Diagnostic {
    use crate::diag::Diagnostic;
    let r = registry();
    if let Some(f) = r.get(name).filter(|f| f.internal) {
        let op = match name {
            "add" => Some("+"),
            "sub" => Some("-"),
            "mul" => Some("*"),
            "div" => Some("/"),
            "mod" => Some("%"),
            _ => None,
        };
        let d = Diagnostic::error(
            span,
            format!("{name} is dform's own, not a function a program calls"),
        );
        return match op {
            Some(op) => d.with_help(format!("write `a {op} b`")),
            None => d.with_help(f.summary.clone()),
        };
    }
    if let Some(help) = gone(name).or_else(|| name.strip_prefix("to_").and_then(gone)) {
        return Diagnostic::error(span, format!("unknown function {name}")).with_help(help);
    }
    let meant = name
        .split_once('_')
        .map(|(p, rest)| format!("{p}.{rest}"))
        .into_iter()
        .chain(name.strip_prefix("to_").map(str::to_string))
        .chain(
            r.functions()
                .filter(|f| !f.internal && f.name.rsplit('.').next() == Some(name))
                .map(|f| f.name.clone()),
        )
        .find(|m| m != name && callable(m));
    let d = Diagnostic::error(span, format!("unknown function {name}"));
    match meant {
        Some(m) => d.with_help(format!("the function is `{m}`")),
        None => d.with_help(format!(
            "a function is named by its package, the type it is about: {} (std/*.df)",
            r.packages().join(", ")
        )),
    }
}

/// Whether `name` is written as a function: one std declares, one it no
/// longer has, or a name in a function package (`inet.contains`).
pub fn is_function_name(name: &str) -> bool {
    get(name).is_some()
        || gone(name).is_some()
        || name
            .split_once('.')
            .is_some_and(|(head, _)| registry().is_package(head))
}

/// What a program writes for a function std no longer has (R-133,
/// R-134): there are no constructors, a type's parts are its fields.
fn gone(name: &str) -> Option<String> {
    // The type, a name for a value of it, an example and its fields.
    let typed = |ty: &str, x: &str, example: &str, fields: &[&str]| {
        let fields = if fields.is_empty() {
            String::new()
        } else {
            let fs: Vec<String> = fields.iter().map(|f| format!("`{x}.{f}`")).collect();
            format!("; its parts are fields, {}", fs.join(", "))
        };
        format!(
            "there are no constructors: a string where {} is wanted is read as one, a \
             parameter, an attribute or an input typed `{ty}`, or a typed `let`, `let {x}: \
             {ty} = {example}`{fields}",
            article(ty)
        )
    };
    Some(match name {
        "inet" => typed("inet", "n", "\"10.0.0.0/16\"", &["addr", "bits"]),
        "ip" => typed("ip", "a", "\"10.0.0.1\"", &[]),
        "iprange" => typed(
            "range(ip)",
            "r",
            "\"10.0.0.10..=10.0.0.99\"",
            &["start", "end", "len"],
        ),
        "time" | "time.parse" => typed("time", "t", "\"2026-10-02T09:00[Europe/Paris]\"", &[]),
        "duration" | "duration.parse" => typed("duration", "d", "30m", &[]),
        "bytes" => typed("bytes", "b", "\"512Mi\"", &[]),
        "cpu" => typed("cpu", "c", "500m", &[]),
        "uri.parse" => typed(
            "uri",
            "u",
            "\"https://example.com\"",
            &["scheme", "host", "port", "path", "query", "fragment"],
        ),
        "oci" | "oci.parse" => typed(
            "oci",
            "r",
            "\"ghcr.io/o/app:1\"",
            &["registry", "repository", "tag", "digest"],
        ),
        "semver" | "semver.parse" => typed(
            "semver",
            "v",
            "\"1.2.3\"",
            &["major", "minor", "patch", "pre"],
        ),
        "inet.prefix_len" => "a network's prefix length is its field `n.bits`".to_string(),
        // No prelude (R-155): every function is named by its package.
        "int" => "an int from a float is named by how it rounds, `int.trunc(f)`, \
                  `int.round(f)`, `int.floor(f)`, `int.ceil(f)`; from a string's text, a typed \
                  position reads it, `let n: int = s`"
            .to_string(),
        "float" => "an int is a float where one is wanted, `let f: float = n`; a string's text \
                    likewise, `let f: float = s`"
            .to_string(),
        "format" => "a template is `str.format(\"%s-%s\", a, b)`, or an interpolation, \
                     `\"${a}-${b}\"`"
            .to_string(),
        "to" => "a quantity in a unit is `quantity.to(q, unit)`".to_string(),
        "declassify" => "a secret leaves on purpose by `secret.declassify(v, reason)`".to_string(),
        "scoped" => "a name in a used module or a copy is `m.x`, `copy.x`".to_string(),
        // Membership is the operator `in` (R-155).
        "inet.contains" => "an address in a network is `a in net`".to_string(),
        "list.contains" => "a value in a list is `v in xs`".to_string(),
        "str.contains" => "a substring of a string is `\"x\" in s`".to_string(),
        // Operators where a type has them (R-134).
        "time.add" => "a time moves by a duration with `t + d` (and `t - d`)".to_string(),
        "time.until" => "the duration from `a` to `b` is `b - a`".to_string(),
        "time.before" => "a time before another is `a < b`".to_string(),
        "semver.compare" => "versions compare with `<`, `==` and `>`: `a < b`".to_string(),
        // One name per idea (R-134).
        "len" | "list.len" | "str.len" => {
            "the length of a list, an object or a string is its field `x.len`".to_string()
        }
        "bytes.to" | "cpu.to" | "duration.total" => {
            "a quantity in a unit is `quantity.to(q, unit)`, the unit as its literals write it \
             (`\"Gi\"`, `\"m\"`, `\"h\"`)"
                .to_string()
        }
        "inet.addr" => "a network's `n`th usable host is `inet.host(net, n)`, its base address \
                        the field `net.addr`"
            .to_string(),
        "hash.short" => "a short digest is `str.slice(hash.sha256(s), 0, n)`".to_string(),
        "random.bytes" => {
            "base64 text of derived bytes is `random.base64(key, length)`".to_string()
        }
        "string" | "str" => {
            "a value's text is an interpolation, `\"${x}\"`; there are no constructors".to_string()
        }
        // R-134: a uri is RFC 3986's, not a browser's url.
        n if n == "url" || n.starts_with("url.") => {
            let f = n.strip_prefix("url.").unwrap_or("");
            format!(
                "the type and its package are `uri` (RFC 3986's generic syntax){}; a string \
                 where a uri is wanted is read as one, `let u: uri = \"https://example.com\"`",
                match f {
                    "" | "parse" => String::new(),
                    "encode" => ": `uri.escape`".to_string(),
                    f => format!(": `uri.{f}`"),
                }
            )
        }
        _ => return None,
    })
}

/// `an inet`, `a time`.
fn article(ty: &str) -> String {
    match ty.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => format!("an `{ty}`"),
        _ => format!("a `{ty}`"),
    }
}

impl Registry {
    /// Read signature files, `(path, text)`; an error names the file and line.
    pub fn load(sources: &[(&str, &str)]) -> Result<Registry, String> {
        let mut by_name: BTreeMap<String, Function> = BTreeMap::new();
        let mut packages: BTreeMap<String, String> = BTreeMap::new();
        for (file, text) in sources {
            for f in parse(file, text)? {
                if let Some(other) = packages.get(&f.package)
                    && other != file
                {
                    return Err(format!(
                        "{file}: package {} is also declared in {other}",
                        f.package
                    ));
                }
                packages.insert(f.package.clone(), file.to_string());
                if let Some(prev) = by_name.get(&f.name) {
                    return Err(format!(
                        "{}:{}: {} is declared twice (also {}:{})",
                        f.file, f.line, f.name, prev.file, prev.line
                    ));
                }
                by_name.insert(f.name.clone(), f);
            }
        }
        Ok(Registry { by_name })
    }

    pub fn get(&self, name: &str) -> Option<&Function> {
        self.by_name.get(name)
    }

    /// Every function, by name.
    pub fn functions(&self) -> impl Iterator<Item = &Function> {
        self.by_name.values()
    }

    /// The function packages, the lowering's aside: the heads of qualified
    /// names.
    pub fn packages(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .by_name
            .values()
            .map(|f| f.package.as_str())
            .filter(|p| *p != LOWERING)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Whether `head` names a function package (`inet` in `inet.subnet`).
    pub fn is_package(&self, head: &str) -> bool {
        head != LOWERING && self.by_name.values().any(|f| f.package == head)
    }
}

/// How many parentheses and braces of `sig` are open.
fn open(sig: &str) -> i32 {
    sig.chars()
        .map(|c| match c {
            '(' | '{' => 1,
            ')' | '}' => -1,
            _ => 0,
        })
        .sum()
}

/// One signature file: `package NAME`, then `fn` lines, each with the
/// `#|` lines above it as its documentation (bare lines its summary,
/// `example:` its example). `#` comments and blank lines are ignored. A
/// signature too long for one line continues on the next lines until its
/// parentheses and braces close (its parameters wrapped, one a line).
pub fn parse(file: &str, text: &str) -> Result<Vec<Function>, String> {
    let mut package: Option<String> = None;
    let mut doc: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((i, raw)) = lines.next() {
        let line = i + 1;
        let err = |m: String| format!("{file}:{line}: {m}");
        let l = raw.trim();
        if let Some(d) = l.strip_prefix("#|") {
            doc.push(d.trim());
            continue;
        }
        if l.is_empty() || l.starts_with('#') {
            doc.clear();
            continue;
        }
        if let Some(rest) = l.strip_prefix("package ") {
            if package.is_some() {
                return Err(err("one package per file".into()));
            }
            let name = rest.trim();
            if !crate::lexer::is_word(name) {
                return Err(err(format!("`{name}` is not a package name")));
            }
            package = Some(name.to_string());
            doc.clear();
            continue;
        }
        let (internal, l) = match l.strip_prefix("internal ") {
            Some(r) => (true, r.trim_start()),
            None => (false, l),
        };
        let Some(sig) = l.strip_prefix("fn ") else {
            return Err(err(format!("expected `package` or `fn`, found `{l}`")));
        };
        let Some(package) = &package else {
            return Err(err("a function before `package`".into()));
        };
        let mut sig = sig.trim().to_string();
        while open(&sig) > 0
            && let Some((_, more)) = lines.next()
        {
            if !sig.ends_with(['(', '{']) {
                sig.push(' ');
            }
            sig.push_str(more.trim());
        }
        // A wrapped list's last parameter keeps its comma.
        let sig = sig.replace(", )", ")");
        let mut f = function(&sig).map_err(err)?;
        f.internal = internal;
        // The lowering's own are bare and written by no program; every
        // other function is named by its package (R-155).
        if package == LOWERING && !internal {
            return Err(err(format!(
                "{} is the lowering's: a function a program calls is named by its \
                 package, in std/*.df",
                f.name
            )));
        }
        f.name = if package == LOWERING {
            f.name
        } else {
            format!("{package}.{}", f.name)
        };
        f.package = package.clone();
        f.file = file.to_string();
        f.line = line;
        let mut summary = Vec::new();
        for d in doc.drain(..) {
            match d.strip_prefix("example:") {
                Some(e) => f.example = e.trim().to_string(),
                None => summary.push(d),
            }
        }
        f.summary = summary.join(" ");
        let params: Vec<String> = f
            .params
            .iter()
            .map(Param::text)
            .chain(f.variadic.then(|| "...".to_string()))
            .collect();
        f.signature = format!(
            "{}({}) -> {}{}",
            f.name,
            params.join(", "),
            f.ret,
            if f.partial { "?" } else { "" }
        );
        out.push(f);
    }
    if package.is_none() {
        return Err(format!("{file}: no `package` line"));
    }
    Ok(out)
}

/// `name(p: T, ...) -> T[?] [flag, ...]`, the part after `fn `. Shared
/// with `fmt` (R-24: a signature file's own normal form).
pub(crate) fn function(sig: &str) -> Result<Function, String> {
    let open = sig.find('(').ok_or("expected `(` after the name")?;
    let name = sig[..open].trim();
    if !crate::lexer::is_word(name) {
        return Err(format!("`{name}` is not a function name"));
    }
    let close = matching(sig, open).ok_or("unclosed `(`")?;
    let mut params = Vec::new();
    let mut variadic = false;
    for p in split_top(&sig[open + 1..close]) {
        if variadic {
            return Err("`...` is the last parameter".into());
        }
        if p == "..." {
            if params.is_empty() {
                return Err("`...` repeats the parameter before it".into());
            }
            variadic = true;
            continue;
        }
        let (n, t) = p
            .split_once(':')
            .ok_or_else(|| format!("parameter `{p}` has no type"))?;
        let (n, t) = (n.trim(), t.trim());
        let (n, optional) = match n.strip_suffix('?') {
            Some(n) => (n, true),
            None => (n, false),
        };
        let (t, default) = match t.split_once('=') {
            Some((t, d)) if optional => (t.trim(), Some(d.trim().to_string())),
            Some(_) => return Err(format!("`{n}` has a default: only an optional one may")),
            None => (t, None),
        };
        if !crate::lexer::is_word(n) || t.is_empty() {
            return Err(format!("`{p}` is not `name: type`"));
        }
        if !optional && params.iter().any(|p: &Param| p.optional) {
            return Err(format!(
                "`{n}` follows an optional parameter: only the last ones may be left out"
            ));
        }
        let param = Param {
            name: n.to_string(),
            ty: t.to_string(),
            optional,
            default,
        };
        if param.default.is_some() && param.default_value().is_none() {
            return Err(format!(
                "`{n}`'s default is an int, a bool or a quoted string"
            ));
        }
        params.push(param);
    }
    let rest = sig[close + 1..].trim();
    let rest = rest
        .strip_prefix("->")
        .ok_or("expected `-> TYPE` after the parameters")?
        .trim();
    // The result type: a name and its balanced brackets.
    let end = type_end(rest);
    let ret = rest[..end].trim().to_string();
    if ret.is_empty() {
        return Err("expected the result type after `->`".into());
    }
    let mut rest = rest[end..].trim();
    let partial = rest.starts_with('?');
    if partial {
        rest = rest[1..].trim();
    }
    let (mut forwards, mut forwards_nulls, mut coeffect) = (false, false, false);
    for flag in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match flag.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["forwards"] => forwards = true,
            ["forwards", "nulls"] => forwards_nulls = true,
            ["reads"] => coeffect = true,
            _ => {
                return Err(format!(
                    "unknown flag `{flag}` (`forwards`, `forwards nulls`, `reads`)"
                ));
            }
        }
    }
    Ok(Function {
        name: name.to_string(),
        package: String::new(),
        params,
        variadic,
        ret,
        partial,
        forwards,
        forwards_nulls,
        internal: false,
        coeffect,
        summary: String::new(),
        example: String::new(),
        signature: String::new(),
        file: String::new(),
        line: 0,
    })
}

fn matching(s: &str, open: usize) -> Option<usize> {
    matching_pair(s, open, '(', ')')
}

/// The index of the `close` that matches the `open` at `at` (nesting
/// counted), or none if `s` is not balanced from there.
fn matching_pair(s: &str, at: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < at) {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// The parts of `s` between its top-level commas, trimmed; none for blank.
fn split_top(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if !s[start..].trim().is_empty() || !out.is_empty() {
        out.push(s[start..].trim());
    }
    out
}

/// Where a type at the start of `s` ends: a name, then `( .. )` if any
/// (`list(string)`), or an object type written out in full
/// (`{ a: string, b: int? }`, a function's return shape, R-6).
fn type_end(s: &str) -> usize {
    if s.starts_with('{') {
        return matching_pair(s, 0, '{', '}').map_or(s.len(), |c| c + 1);
    }
    let name = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(s.len());
    if s[name..].starts_with('(') {
        matching(s, name).map_or(s.len(), |c| c + 1)
    } else {
        name
    }
}

/// A function's body: its arguments' values to its value, or none.
pub type Body = fn(&[Value]) -> Option<Value>;

/// `__path(v, path)`: the part of `v` at `path`, followed through `v`
/// itself and copied only where it ends (a 50 KB document read for its
/// `metadata.name` copies the name).
pub fn path_of(v: &Value, path: &str) -> Option<Value> {
    use std::borrow::Cow;
    // A part of a value not known yet (a document a host has not
    // written, R-153) is not known yet either: the same null.
    if let Value::Null { .. } = v {
        return Some(v.clone());
    }
    let mut v: Cow<Value> = Cow::Borrowed(v);
    for seg in crate::address::path_keys(path) {
        // A url's or an image reference's parts read as an object's
        // (`u.host`, `r.digest`).
        if let Value::Oci(r) = &*v {
            v = Cow::Owned(crate::value::OciRef::parse(r).ok()?.parts());
        }
        if let Some(parts) = crate::value::parts(&v) {
            v = Cow::Owned(parts);
        }
        v = match v {
            Cow::Borrowed(Value::Obj(m)) => Cow::Borrowed(m.get(&seg)?),
            Cow::Owned(Value::Obj(mut m)) => Cow::Owned(m.remove(&seg)?),
            _ => return None,
        };
    }
    Some(v.into_owned())
}

/// Where [`path_of`] stops at a value that is not an object (R-185): the
/// segments walked before it and that value. `None` where the walk ends
/// at a field an object does not have, which is no value, or does not
/// stop.
pub fn not_an_object(v: &Value, path: &str) -> Option<(Vec<String>, Value)> {
    let mut v = v.clone();
    let mut walked = Vec::new();
    for seg in crate::address::path_keys(path) {
        if let Value::Null { .. } = v {
            return None;
        }
        if let Value::Oci(r) = &v {
            v = crate::value::OciRef::parse(r).ok()?.parts();
        }
        if let Some(parts) = crate::value::parts(&v) {
            v = parts;
        }
        let Value::Obj(mut m) = v else {
            return Some((walked, v));
        };
        v = m.remove(&seg)?;
        walked.push(seg);
    }
    None
}

/// The body of the function `name` declares in `std/*.df`, by its
/// qualified name; a test keeps every declared function's body in step.
pub fn body(name: &str) -> Option<Body> {
    BODIES.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

/// Every function's body, by its qualified name.
pub const BODIES: &[(&str, Body)] = &[
    ("add", |a| {
        num2(a, |x, y| Some(x + y), |x, y| x + y).or_else(|| measured(a, false))
    }),
    ("sub", |a| {
        num2(a, |x, y| Some(x - y), |x, y| x - y).or_else(|| measured(a, true))
    }),
    ("mul", |a| {
        num2(a, |x, y| Some(x * y), |x, y| x * y).or_else(|| match a {
            [Value::Quantity(q), Value::Int(n)] | [Value::Int(n), Value::Quantity(q)] => {
                q.checked_mul(*n).map(Value::Quantity)
            }
            _ => None,
        })
    }),
    ("div", |a| {
        num2(a, |x, y| (y != 0).then(|| x / y), |x, y| x / y).or_else(|| match a {
            [Value::Quantity(q), Value::Int(n)] => q.checked_div(*n).map(Value::Quantity),
            [Value::Quantity(p), Value::Quantity(q)] => p.ratio(q).map(Value::Int),
            _ => None,
        })
    }),
    ("mod", |a| {
        num2(a, |x, y| (y != 0).then(|| x % y), |x, y| x % y)
    }),
    // An int from a float, named by how it rounds (R-155; DESIGN.org
    // "Silent string-to-int coercion"): none past an int's range.
    ("int.trunc", |a| rounded(a, f64::trunc)),
    ("int.round", |a| rounded(a, f64::round)),
    ("int.floor", |a| rounded(a, f64::floor)),
    ("int.ceil", |a| rounded(a, f64::ceil)),
    // An ambiguous quantity no position read has no value; the compiler
    // says so where the schema is known (`types::read`).
    (crate::types::AMBIGUOUS, |_| None),
    (crate::range::LOWERED, |a| match a {
        [start, end, Value::Bool(inclusive)] => {
            crate::range::Range::new(start.clone(), end.clone(), *inclusive)
                .ok()
                .map(Value::from)
        }
        _ => None,
    }),
    ("time.format", |a| match a {
        [Value::Time(t), Value::Str(layout)] => t.format(layout).map(Value::Str),
        _ => None,
    }),
    ("time.in_zone", |a| match a {
        [Value::Time(t), Value::Str(zone)] => t.in_zone(zone).map(Value::Time),
        _ => None,
    }),
    ("quantity.to", unit_of),
    (crate::address::FORMAT, |a| {
        let fmt = a.first()?.as_str()?;
        let mut out = String::new();
        let mut parts = fmt.split("%s");
        out.push_str(parts.next().unwrap_or(""));
        for (i, p) in parts.enumerate() {
            // A list and an object have no text (R-155: `${..}` is over
            // the types that have one).
            match a.get(i + 1)? {
                Value::List(_) | Value::Obj(_) => return None,
                v => out.push_str(&value_to_string(v)),
            }
            out.push_str(p);
        }
        Some(Value::Str(out))
    }),
    (crate::address::LEN, len_of),
    (crate::address::REF, |a| match a {
        [Value::Str(t), Value::Str(n), Value::Str(p)] => Some(Value::Ref {
            typ: t.clone(),
            name: n.clone(),
            attr: p.clone(),
        }),
        // `ref(r)` written out (R-43): the reference itself.
        [r @ Value::Ref { .. }] => Some(r.clone()),
        _ => None,
    }),
    (crate::address::CLOUD_REF, |a| match a {
        [Value::Str(t), Value::Str(n), Value::Str(p)] => Some(Value::CloudRef {
            typ: t.clone(),
            name: n.clone(),
            attr: p.clone(),
        }),
        _ => None,
    }),
    // A name that is already an address (another copy's resource, read
    // through its output) is itself (R-65).
    (crate::address::SCOPED, |a| match a {
        [_, Value::Str(name)] if crate::address::is_scoped(name) => Some(Value::Str(name.clone())),
        [scope, name] => Some(Value::Str(crate::address::scoped(
            &value_to_string(scope),
            &value_to_string(name),
        ))),
        _ => None,
    }),
    // A resource's name interpolated at run time as one segment of its
    // address (R-112): quoted when it holds a dot (`"a.b"`).
    (crate::address::NAME_SEGMENT, |a| match a {
        [Value::Str(name)] => Some(Value::Str(crate::address::name_segment(name).into_owned())),
        _ => None,
    }),
    // A resource's value body (R-126): an object, a document of the type.
    (crate::address::RESOURCE_BODY, |a| match a {
        [v @ Value::Obj(_)] => Some(v.clone()),
        _ => None,
    }),
    // E DR-19: `secret.declassify(V, Reason)` is `V`; the static pass
    // reads it as public, and `declassified/2` records it (`transform`).
    ("secret.declassify", |a| match a {
        [v, _] => Some(v.clone()),
        _ => None,
    }),
    // The lowering's null for (T, A, P) (E §2.5): class and type come
    // from the schema row the rule was expanded from.
    ("__null", |a| match a {
        [t, n, p, class, ty] => Some(Value::Null {
            label: crate::value::null_label(t.as_str()?, &value_to_string(n), p.as_str()?),
            class: crate::value::NullClass::parse(class.as_str()?)?,
            ty: ty.as_str()?.to_string(),
        }),
        _ => None,
    }),
    ("__label", |a| match a {
        [t, n, p] => Some(Value::Str(crate::value::null_label(
            t.as_str()?,
            &value_to_string(n),
            p.as_str()?,
        ))),
        _ => None,
    }),
    // `ref(T, A, "a.b")` after the rewrite: walk the rest of the path
    // inside the top-level attribute's value.
    ("__under", |a| match a {
        [Value::Str(path), Value::Str(prefix), v] => {
            let rest = path.strip_prefix(prefix.as_str())?.strip_prefix('.')?;
            rest.split('.').rev().try_fold(v.clone(), |v, k| {
                (!k.is_empty()).then(|| Value::Obj(BTreeMap::from([(k.to_string(), v)])))
            })
        }
        _ => None,
    }),
    // A typed position over a computed value (R-134): a string read as
    // the type, a value of it itself; none for one that is not, which
    // the position reports (`engine::head_error`).
    ("__as", |a| match a {
        // A reference is read when the apply resolves it.
        [
            v @ (Value::Ref { .. } | Value::CloudRef { .. } | Value::Null { .. }),
            _,
        ] => Some(v.clone()),
        [v, Value::Str(ty)] => crate::value::read_typed(ty, v).ok(),
        _ => None,
    }),
    // `{ "${k}": V }` (After R-178): an object whose keys are computed.
    (OBJECT, |a| {
        if a.len() % 2 != 0 {
            return None;
        }
        let mut m = BTreeMap::new();
        for kv in a.chunks(2) {
            let k = match &kv[0] {
                Value::Str(k) => k.clone(),
                // A key not known yet: neither is the object.
                n @ Value::Null { .. } => return Some(n.clone()),
                _ => return None,
            };
            if m.insert(k, kv[1].clone()).is_some() {
                return None;
            }
        }
        Some(Value::Obj(m))
    }),
    (MERGE, |a| {
        let mut m = BTreeMap::new();
        for part in a {
            match part {
                Value::Obj(fields) => m.extend(fields.clone()),
                _ => return None,
            }
        }
        Some(Value::Obj(m))
    }),
    (CONCAT, |a| {
        let mut out = Vec::new();
        for part in a {
            match part {
                Value::List(xs) => out.extend(xs.iter().cloned()),
                Value::Range(r) => out.extend(r.members().ok()?),
                _ => return None,
            }
        }
        Some(Value::List(out))
    }),
    (REST, |a| match a {
        [n @ Value::Null { .. }, ..] => Some(n.clone()),
        [Value::Obj(m), keys @ ..] => {
            let mut m = m.clone();
            for k in keys {
                m.remove(k.as_str()?);
            }
            Some(Value::Obj(m))
        }
        _ => None,
    }),
    ("__known", |a| match a {
        [_] => Some(Value::Bool(true)),
        _ => None,
    }),
    ("__path", |a| match a {
        [v, path] => path_of(v, path.as_str()?),
        _ => None,
    }),
    ("inet.subnet", |a| match a {
        [net, bits, n] => {
            let (addr, prefix) = as_ipnet(net)?;
            let (nb, nn) = (as_i64(bits)?, as_i64(n)?);
            if nb < 0 || nn < 0 {
                return None;
            }
            let new_prefix = (prefix as i64) + nb;
            if new_prefix > 32 {
                return None;
            }
            let shift = 32 - (new_prefix as u32);
            Some(Value::IpNet {
                addr: addr + ((nn as u32) << shift),
                prefix: new_prefix as u8,
            })
        }
        _ => None,
    }),
    ("inet.host", |a| match a {
        [net, n] => {
            let (start, end) = ipnet_range(net)?;
            // usable hosts exclude network + broadcast
            if end <= start + 1 {
                return None;
            }
            let idx = as_i64(n)?;
            if idx < 0 {
                return None;
            }
            let ip = (start + 1).checked_add(u32::try_from(idx).ok()?)?;
            (ip < end).then_some(Value::Ip(ip))
        }
        _ => None,
    }),
    ("inet.overlaps", |a| match a {
        [x, y] => {
            let (a0, a1) = ipnet_range(x)?;
            let (b0, b1) = ipnet_range(y)?;
            Some(Value::Bool(a0 <= b1 && b0 <= a1))
        }
        _ => None,
    }),
    ("ip.unspecified", |a| match a {
        [ip] => Some(Value::Bool(as_ip_u32(ip)? == 0)),
        _ => None,
    }),
    ("int.range", |a| match a {
        [Value::Int(lo), Value::Int(hi), Value::Int(step)] if *step != 0 => {
            let mut out = Vec::new();
            let mut i = *lo;
            while (*step > 0 && i < *hi) || (*step < 0 && i > *hi) {
                out.push(Value::Int(i));
                i = i.checked_add(*step)?;
            }
            Some(Value::List(out))
        }
        _ => None,
    }),
    ("str.lower", |a| match a {
        [Value::Str(s)] => Some(Value::Str(s.to_lowercase())),
        _ => None,
    }),
    ("str.upper", |a| match a {
        [Value::Str(s)] => Some(Value::Str(s.to_uppercase())),
        _ => None,
    }),
    ("str.dedent", |a| match a {
        [Value::Str(s)] => Some(Value::Str(dedent(s))),
        _ => None,
    }),
    ("str.split", |a| match a {
        [Value::Str(s), Value::Str(sep)] if !sep.is_empty() => Some(Value::List(
            s.split(sep.as_str())
                .map(|x| Value::Str(x.to_string()))
                .collect(),
        )),
        // At most `limit` splits, the first ones: `limit + 1` parts.
        [Value::Str(s), Value::Str(sep), Value::Int(limit)] if !sep.is_empty() && *limit >= 0 => {
            let parts = usize::try_from(*limit).ok()?.checked_add(1)?;
            Some(Value::List(
                s.splitn(parts, sep.as_str())
                    .map(|x| Value::Str(x.to_string()))
                    .collect(),
            ))
        }
        _ => None,
    }),
    ("list.join", |a| match a {
        [Value::List(xs), Value::Str(sep)] => {
            let parts: Option<Vec<String>> = xs.iter().map(scalar_text).collect();
            Some(Value::Str(parts?.join(sep)))
        }
        _ => None,
    }),
    ("random.password", crate::functions::random::password),
    ("random.base64", crate::functions::random::base64),
    ("random.id", crate::functions::random::id),
    ("random.uuid", crate::functions::random::uuid),
    ("random.signing_key", crate::functions::random::signing_key),
    ("random.verify_key", crate::functions::random::verify_key),
    ("random.generation", crate::functions::random::generation),
    ("str.trim", |a| match a {
        [Value::Str(s)] => Some(Value::Str(s.trim().to_string())),
        _ => None,
    }),
    ("str.replace", |a| match a {
        [Value::Str(s), Value::Str(from), Value::Str(to)] if !from.is_empty() => {
            Some(Value::Str(s.replace(from.as_str(), to)))
        }
        _ => None,
    }),
    ("str.starts_with", |a| match a {
        [Value::Str(s), Value::Str(p)] => Some(Value::Bool(s.starts_with(p.as_str()))),
        _ => None,
    }),
    ("str.ends_with", |a| match a {
        [Value::Str(s), Value::Str(p)] => Some(Value::Bool(s.ends_with(p.as_str()))),
        _ => None,
    }),
    ("str.pad_left", |a| match a {
        [Value::Str(s), Value::Int(width), Value::Str(pad)] if !pad.is_empty() => {
            Some(Value::Str(pad_to(s, *width, pad, true)?))
        }
        _ => None,
    }),
    ("str.pad_right", |a| match a {
        [Value::Str(s), Value::Int(width), Value::Str(pad)] if !pad.is_empty() => {
            Some(Value::Str(pad_to(s, *width, pad, false)?))
        }
        _ => None,
    }),
    ("str.slice", |a| match a {
        [Value::Str(s), Value::Int(start)] => str_slice(s, *start, None),
        [Value::Str(s), Value::Int(start), Value::Int(end)] => str_slice(s, *start, Some(*end)),
        _ => None,
    }),
    ("list.sort", |a| match a {
        [Value::List(xs)] => {
            let mut xs = xs.clone();
            xs.sort();
            Some(Value::List(xs))
        }
        _ => None,
    }),
    ("list.sort_by", |a| match a {
        [Value::List(xs), Value::Str(field)] => {
            let mut keyed: Vec<(&Value, &Value)> = xs
                .iter()
                .map(|x| match x {
                    Value::Obj(m) => m.get(field.as_str()).map(|v| (v, x)),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            keyed.sort_by_key(|(a, _)| *a);
            Some(Value::List(
                keyed.into_iter().map(|(_, x)| x.clone()).collect(),
            ))
        }
        _ => None,
    }),
    ("list.unique", |a| match a {
        [Value::List(xs)] => {
            let mut seen = std::collections::HashSet::new();
            Some(Value::List(
                xs.iter()
                    .filter(|x| seen.insert((*x).clone()))
                    .cloned()
                    .collect(),
            ))
        }
        _ => None,
    }),
    ("list.flatten", |a| match a {
        [Value::List(xs)] => {
            let mut out = Vec::new();
            for x in xs {
                let Value::List(inner) = x else { return None };
                out.extend(inner.iter().cloned());
            }
            Some(Value::List(out))
        }
        _ => None,
    }),
    ("list.zip", |a| match a {
        [Value::List(x), Value::List(y)] => Some(Value::List(
            x.iter()
                .zip(y.iter())
                .map(|(a, b)| Value::List(vec![a.clone(), b.clone()]))
                .collect(),
        )),
        _ => None,
    }),
    ("list.min", |a| match a {
        [Value::List(xs)] => xs.iter().min().cloned(),
        _ => None,
    }),
    ("list.max", |a| match a {
        [Value::List(xs)] => xs.iter().max().cloned(),
        _ => None,
    }),
    ("list.sum", |a| match a {
        [Value::List(xs)] => xs.iter().try_fold(Value::Int(0), |total, x| match x {
            Value::Int(_) | Value::Float(_) => {
                num2(&[total, x.clone()], i64::checked_add, |x, y| x + y)
            }
            _ => None,
        }),
        _ => None,
    }),
    ("list.first", |a| match a {
        [Value::List(xs)] => xs.first().cloned(),
        _ => None,
    }),
    ("list.last", |a| match a {
        [Value::List(xs)] => xs.last().cloned(),
        _ => None,
    }),
    ("regex.match", |a| match a {
        [Value::Str(s), Value::Str(re)] => {
            Some(Value::Bool(regex::Regex::new(re).ok()?.is_match(s)))
        }
        _ => None,
    }),
    ("regex.capture", |a| match a {
        [Value::Str(s), Value::Str(re), Value::Int(n)] => {
            let re = regex::Regex::new(re).ok()?;
            let caps = re.captures(s)?;
            caps.get(usize::try_from(*n).ok()?)
                .map(|m| Value::Str(m.as_str().to_string()))
        }
        _ => None,
    }),
    ("regex.replace", |a| match a {
        [Value::Str(s), Value::Str(re), Value::Str(with)] => Some(Value::Str(
            regex::Regex::new(re)
                .ok()?
                .replace_all(s, with.as_str())
                .into_owned(),
        )),
        _ => None,
    }),
    ("semver.satisfies", |a| match a {
        [Value::Semver(v), Value::Str(range)] => {
            let req = semver::VersionReq::parse(range).ok()?;
            Some(Value::Bool(req.matches(&v.0)))
        }
        _ => None,
    }),
    // A predicate as well: a string that is no reference is not pinned.
    ("oci.pinned", |a| match a {
        [r] => Some(Value::Bool(as_oci(r).is_some_and(|r| r.digest.is_some()))),
        _ => None,
    }),
    // A new tag names other content: the digest, which pinned the old,
    // goes with the old tag.
    ("oci.with_tag", |a| match a {
        [r, Value::Str(t)] => {
            crate::value::oci_tag(t).ok()?;
            let mut r = as_oci(r)?;
            r.tag = Some(t.clone());
            r.digest = None;
            Some(r.value())
        }
        _ => None,
    }),
    ("oci.with_digest", |a| match a {
        [r, Value::Str(d)] => {
            crate::value::oci_digest(d).ok()?;
            let mut r = as_oci(r)?;
            r.digest = Some(d.clone());
            Some(r.value())
        }
        _ => None,
    }),
    ("oci.with_registry", |a| match a {
        [r, Value::Str(reg)] => {
            crate::value::oci_registry(reg).ok()?;
            let mut r = as_oci(r)?;
            r.set_registry(Some(reg.clone()));
            Some(r.value())
        }
        _ => None,
    }),
    ("hash.sha256", |a| match a {
        [Value::Str(s)] => Some(Value::Str(crate::approval::sha256_hex(s.as_bytes()))),
        _ => None,
    }),
    ("base64.encode", |a| match a {
        [Value::Str(s)] => {
            use base64::Engine;
            Some(Value::Str(
                base64::engine::general_purpose::STANDARD.encode(s.as_bytes()),
            ))
        }
        _ => None,
    }),
    ("base64.decode", |a| match a {
        [Value::Str(s)] => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .ok()?;
            String::from_utf8(bytes).ok().map(Value::Str)
        }
        _ => None,
    }),
    ("uri.join", |a| match a {
        [u, Value::Str(segment)] => with_uri(u, |u| {
            u.path = join_slash(&u.path, segment);
            if !u.path.starts_with('/') && u.host.is_some() {
                u.path.insert(0, '/');
            }
            Some(())
        }),
        _ => None,
    }),
    ("uri.with_scheme", |a| match a {
        [u, Value::Str(scheme)] => with_uri(u, |u| {
            u.scheme = scheme.clone();
            Some(())
        }),
        _ => None,
    }),
    ("uri.with_user", |a| match a {
        [u, Value::Str(user)] => {
            let u = as_uri(u)?.with_authority();
            with_uri(&Value::Uri(Box::new(u)), |u| {
                u.user = Some(utf8_percent_encode(user, USERINFO).to_string());
                u.host.get_or_insert_default();
                Some(())
            })
        }
        _ => None,
    }),
    ("uri.with_password", |a| match a {
        [u, Value::Str(pw)] => {
            let u = as_uri(u)?.with_authority();
            with_uri(&Value::Uri(Box::new(u)), |u| {
                u.password = Some(utf8_percent_encode(pw, USERINFO).to_string());
                u.user.get_or_insert_default();
                u.host.get_or_insert_default();
                Some(())
            })
        }
        _ => None,
    }),
    ("uri.with_host", |a| match a {
        [u, Value::Str(host)] => {
            let u = as_uri(u)?.with_authority();
            with_uri(&Value::Uri(Box::new(u)), |u| {
                u.host = Some(host.clone());
                Some(())
            })
        }
        _ => None,
    }),
    ("uri.with_port", |a| match a {
        [u, Value::Int(port)] => {
            let p = u16::try_from(*port).ok()?;
            let u = as_uri(u)?.with_authority();
            with_uri(&Value::Uri(Box::new(u)), |u| {
                u.port = Some(p);
                u.host.get_or_insert_default();
                Some(())
            })
        }
        _ => None,
    }),
    ("uri.with_path", |a| match a {
        [u, Value::Str(path)] => with_uri(u, |u| {
            u.path = path.clone();
            if !u.path.is_empty() && !u.path.starts_with('/') && u.host.is_some() {
                u.path.insert(0, '/');
            }
            Some(())
        }),
        _ => None,
    }),
    ("uri.with_query", |a| match a {
        [u, Value::Obj(q)] => {
            let pairs: Option<Vec<String>> = q
                .iter()
                .map(|(k, v)| match v {
                    Value::Str(s) => Some(format!(
                        "{}={}",
                        utf8_percent_encode(k, URL_COMPONENT),
                        utf8_percent_encode(s, URL_COMPONENT)
                    )),
                    _ => None,
                })
                .collect();
            let pairs = pairs?;
            with_uri(u, |u| {
                u.query = (!pairs.is_empty()).then(|| pairs.join("&"));
                Some(())
            })
        }
        _ => None,
    }),
    ("uri.with_fragment", |a| match a {
        [u, Value::Str(f)] => with_uri(u, |u| {
            u.fragment = Some(f.clone());
            Some(())
        }),
        _ => None,
    }),
    ("uri.escape", |a| match a {
        [Value::Str(s)] => Some(Value::Str(
            utf8_percent_encode(s, URL_COMPONENT).to_string(),
        )),
        _ => None,
    }),
    ("path.join", |a| match a {
        [Value::List(parts)] => {
            let mut out = String::new();
            for v in parts {
                let Value::Str(s) = v else { return None };
                out = join_slash(&out, s);
            }
            Some(Value::Str(out))
        }
        _ => None,
    }),
    ("path.dir", |a| match a {
        [Value::Str(p)] => Some(Value::Str(
            match p.rfind('/') {
                Some(0) => "/",
                Some(i) => &p[..i],
                None => ".",
            }
            .to_string(),
        )),
        _ => None,
    }),
    ("path.base", |a| match a {
        [Value::Str(p)] => Some(Value::Str(
            match p.rfind('/') {
                Some(i) => &p[i + 1..],
                None => p.as_str(),
            }
            .to_string(),
        )),
        _ => None,
    }),
    ("path.ext", |a| match a {
        [Value::Str(p)] => {
            let base = match p.rfind('/') {
                Some(i) => &p[i + 1..],
                None => p.as_str(),
            };
            Some(Value::Str(match base.rfind('.') {
                Some(0) | None => String::new(),
                Some(i) => base[i..].to_string(),
            }))
        }
        _ => None,
    }),
    ("path.rel", |a| match a {
        [Value::Str(p), Value::Str(base)] => rel_path(base, p).map(Value::Str),
        _ => None,
    }),
    ("path.clean", |a| match a {
        [Value::Str(p)] => Some(Value::Str(clean_path(p))),
        _ => None,
    }),
    ("json.decode", |a| match a {
        [Value::Str(s)] => crate::tables::document("json", s).ok(),
        _ => None,
    }),
    ("json.encode", |a| match a {
        [v] if encodable(v) => serde_json::to_string(&crate::spell::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
    ("yaml.decode", |a| match a {
        [Value::Str(s)] => crate::tables::document("yaml", s).ok(),
        _ => None,
    }),
    ("yaml.encode", |a| match a {
        [v] if encodable(v) => serde_yaml::to_string(&crate::spell::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
    ("csv.decode", |a| match a {
        [Value::Str(s)] => crate::tables::document_of("csv", s).ok(),
        _ => None,
    }),
    ("csv.encode", |a| match a {
        [Value::List(rows)] => crate::tables::csv_text(rows).map(Value::Str),
        _ => None,
    }),
    ("toml.decode", |a| match a {
        [Value::Str(s)] => crate::tables::document("toml", s).ok(),
        _ => None,
    }),
    ("toml.encode", |a| match a {
        [v] if encodable(v) => toml::to_string(&crate::spell::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
];

/// `str.dedent`: the indentation every non-blank line shares (the same
/// spaces and tabs) removed, blank lines emptied, and a line break at the
/// very start dropped (the one after a literal's opening quote).
fn dedent(s: &str) -> String {
    fn indent(l: &str) -> &str {
        &l[..l.len() - l.trim_start_matches([' ', '\t']).len()]
    }
    let s = s.strip_prefix('\n').unwrap_or(s);
    let blank = |l: &str| indent(l).len() == l.len();
    let mut lines = s.split('\n').filter(|l| !blank(l));
    let first = lines.next().map(indent).unwrap_or("");
    let margin = lines.fold(first, |m, l| {
        let n = m
            .bytes()
            .zip(indent(l).bytes())
            .take_while(|(a, b)| a == b)
            .count();
        &m[..n]
    });
    s.split('\n')
        .map(|l| if blank(l) { "" } else { &l[margin.len()..] })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A value as text: a string bare, a reference as the source names it.
pub(crate) fn value_to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(_) => "<list>".to_string(),
        Value::Obj(_) => "<obj>".to_string(),
        Value::Ip(n) => crate::value::u32_to_ipv4(*n),
        Value::IpNet { addr, prefix } => crate::value::ipnet_to_string(*addr, *prefix),
        Value::Range(r) => r.to_string(),
        // H-16: a reference reads as the source names it, `T["A"].path`.
        Value::Ref { typ, name, attr } => crate::address::Address {
            typ: typ.clone(),
            name: name.clone(),
        }
        .attr(attr),
        Value::CloudRef { typ, name, attr } => format!("cloud_ref({typ},{name},{attr})"),
        Value::Null { label, .. } => format!("?{label}"),
        Value::Quantity(q) => q.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Uri(u) => u.to_string(),
        Value::Oci(u) => u.clone(),
        Value::Semver(v) => v.to_string(),
    }
}

/// `int.trunc` and its kin: the float rounded by `f` as an int (an int
/// is itself); none past an int's range.
fn rounded(a: &[Value], f: fn(f64) -> f64) -> Option<Value> {
    match a {
        [Value::Int(i)] => Some(Value::Int(*i)),
        [Value::Float(x)] => {
            let t = f(x.get());
            (t >= i64::MIN as f64 && t < i64::MAX as f64).then_some(Value::Int(t as i64))
        }
        _ => None,
    }
}

fn len_of(a: &[Value]) -> Option<Value> {
    match a {
        [v] => len(v).map(Value::Int),
        _ => None,
    }
}

/// `.len` of a value: a list's elements, an object's fields, a string's
/// characters, an int range's or an ip range's members; none of anything
/// else.
pub(crate) fn len(v: &Value) -> Option<i64> {
    match v {
        Value::List(xs) => Some(xs.len() as i64),
        Value::Obj(m) => Some(m.len() as i64),
        Value::Str(s) => Some(s.chars().count() as i64),
        // A discrete range's members (R-180).
        Value::Range(r) => r.count(),
        _ => None,
    }
}

/// A quantity as a whole number of a unit (`to`), of its own dimension
/// only.
fn unit_of(a: &[Value]) -> Option<Value> {
    match a {
        [Value::Quantity(q), Value::Str(unit)] => q.in_unit(unit).map(Value::Int),
        _ => None,
    }
}

/// `a + b` (`a - b`) of quantities of one dimension, of a time and a
/// duration (R-66, R-62), and `b - a` of two times; none across
/// dimensions.
fn measured(a: &[Value], sub: bool) -> Option<Value> {
    use crate::quantity::Quantity;
    // A string beside a time or a duration is read as a time, as the
    // other side of an operator gives a literal its type (R-134:
    // `cert.not_after - now` over a string attribute).
    let time = |v: &Value| match v {
        Value::Str(_) => crate::value::read_typed("time", v).ok(),
        v => Some(v.clone()),
    };
    match a {
        [
            x @ Value::Str(_),
            y @ (Value::Time(_) | Value::Quantity(Quantity::Duration(_))),
        ]
        | [x @ Value::Time(_), y @ Value::Str(_)] => {
            return measured(&[time(x)?, time(y)?], sub);
        }
        _ => {}
    }
    match a {
        [Value::Quantity(x), Value::Quantity(y)] => match sub {
            true => x.checked_sub(y),
            false => x.checked_add(y),
        }
        .map(Value::Quantity),
        [Value::Time(t), Value::Quantity(Quantity::Duration(d))] => {
            t.add(if sub { d.negate() } else { *d }).map(Value::Time)
        }
        [Value::Quantity(Quantity::Duration(d)), Value::Time(t)] if !sub => {
            t.add(*d).map(Value::Time)
        }
        // `b - a`: the exact duration from `a` to `b` (R-134).
        [Value::Time(b), Value::Time(a)] if sub => {
            a.until(b).map(|d| Value::Quantity(Quantity::Duration(d)))
        }
        _ => None,
    }
}

/// A function of two numbers (R-75): of two ints `int`, an int; of a
/// float and a number `float`, the int promoted, and no value when the
/// result is not finite (a division by zero).
fn num2(
    a: &[Value],
    int: fn(i64, i64) -> Option<i64>,
    float: fn(f64, f64) -> f64,
) -> Option<Value> {
    let f = |v: &Value| match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(f.get()),
        _ => None,
    };
    match a {
        [Value::Int(x), Value::Int(y)] => int(*x, *y).map(Value::Int),
        [x, y] => crate::value::Float::new(float(f(x)?, f(y)?)).map(Value::Float),
        _ => None,
    }
}

/// Arithmetic takes integers only; a string is converted with `int`.
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

fn as_ip_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Ip(n) => Some(*n),
        Value::Str(s) => crate::value::ipv4_to_u32(s),
        _ => None,
    }
}

/// A uri argument: a uri, or a string read as one (a string in a `uri`
/// position parses at the edge).
fn as_uri(v: &Value) -> Option<crate::uri::Uri> {
    match v {
        Value::Uri(u) => Some((**u).clone()),
        Value::Str(s) => crate::uri::Uri::parse(s).ok(),
        _ => None,
    }
}

/// An image reference argument's parts: an `oci`, or a string read as
/// one (a string in an `oci` position parses at the edge).
fn as_oci(v: &Value) -> Option<crate::value::OciRef> {
    match v {
        Value::Oci(r) | Value::Str(r) => crate::value::OciRef::parse(r).ok(),
        _ => None,
    }
}

/// The uri `u` with `set` applied (`uri.with_host`, ..), read again so
/// it is normalized as a parsed one is: a uri, or no value when `u` is
/// not one or the part does not fit.
fn with_uri(u: &Value, set: impl FnOnce(&mut crate::uri::Uri) -> Option<()>) -> Option<Value> {
    let mut u = as_uri(u)?;
    set(&mut u)?;
    u.reparse().ok().map(|u| Value::Uri(Box::new(u)))
}

fn as_ipnet(v: &Value) -> Option<(u32, u8)> {
    match v {
        Value::IpNet { addr, prefix } => Some((*addr, *prefix)),
        Value::Str(s) => crate::value::parse_ipnet(s),
        _ => None,
    }
}

fn ipnet_range(v: &Value) -> Option<(u32, u32)> {
    let (addr, prefix) = as_ipnet(v)?;
    let host_bits = 32 - (prefix as u32);
    let size = if host_bits == 32 {
        u32::MAX
    } else {
        (1u64 << host_bits) as u32
    };
    let end = addr.wrapping_add(size.wrapping_sub(1));
    Some((addr, end))
}

/// A scalar's text (`string`, `list.join`, `str.format`'s args): a list,
/// an object, a reference and a null have none.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Quantity(_)
        | Value::Time(_)
        | Value::Uri(_)
        | Value::Oci(_)
        | Value::Semver(_)
        | Value::Range(_) => v.typed_text(),
        Value::Ip(_) | Value::IpNet { .. } => Some(spell::value(v)),
        _ => None,
    }
}

/// `s` padded with `pad` (repeated, truncated to fit) to at least
/// `width` characters, on the left or the right (`str.pad_left`,
/// `str.pad_right`).
fn pad_to(s: &str, width: i64, pad: &str, left: bool) -> Option<String> {
    let width = usize::try_from(width).ok()?;
    let have = s.chars().count();
    if have >= width {
        return Some(s.to_string());
    }
    let need = width - have;
    let filler: String = pad.chars().cycle().take(need).collect();
    Some(if left {
        format!("{filler}{s}")
    } else {
        format!("{s}{filler}")
    })
}

/// `s`'s characters from `start` up to `end` (to the end when left
/// out); none past its length or for an end before its start
/// (`str.slice`).
fn str_slice(s: &str, start: i64, end: Option<i64>) -> Option<Value> {
    let chars: Vec<char> = s.chars().collect();
    let start = usize::try_from(start).ok()?;
    let end = match end {
        Some(e) => usize::try_from(e).ok()?,
        None => chars.len(),
    };
    if start > end || end > chars.len() {
        return None;
    }
    Some(Value::Str(chars[start..end].iter().collect()))
}

/// A uri component's safe characters (`uri.escape`, a query pair's
/// parts): alphanumerics and the unreserved punctuation (RFC 3986),
/// everything else percent-encoded.
static URL_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// A userinfo part's safe characters: the unreserved and the
/// sub-delimiters, `:` escaped (it separates the user from the password).
static USERINFO: &percent_encoding::AsciiSet = &URL_COMPONENT
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=');

use percent_encoding::utf8_percent_encode;

/// `a` and `b` joined by exactly one `/`, whatever either side already has.
fn join_slash(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    if b.is_empty() {
        return a.to_string();
    }
    format!(
        "{}/{}",
        a.strip_suffix('/').unwrap_or(a),
        b.strip_prefix('/').unwrap_or(b)
    )
}

/// `p`'s `.` and `..` segments resolved and its repeated slashes
/// collapsed (`path.clean`), without touching a filesystem.
fn clean_path(p: &str) -> String {
    let abs = p.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." if stack.last().is_some_and(|s| *s != "..") => {
                stack.pop();
            }
            ".." if abs => {}
            seg => stack.push(seg),
        }
    }
    let joined = stack.join("/");
    let out = if abs { format!("/{joined}") } else { joined };
    if out.is_empty() { ".".to_string() } else { out }
}

/// `to` relative to the directory `from` (`path.rel`); none when one is
/// absolute and the other is not.
fn rel_path(from: &str, to: &str) -> Option<String> {
    let (from_c, to_c) = (clean_path(from), clean_path(to));
    if from_c.starts_with('/') != to_c.starts_with('/') {
        return None;
    }
    let fs: Vec<&str> = from_c.split('/').filter(|s| !s.is_empty()).collect();
    let ts: Vec<&str> = to_c.split('/').filter(|s| !s.is_empty()).collect();
    let common = fs.iter().zip(ts.iter()).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = (common..fs.len()).map(|_| "..".to_string()).collect();
    parts.extend(ts[common..].iter().map(|s| s.to_string()));
    Some(if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    })
}

/// Whether `v` has no reference and no null anywhere inside it
/// (`json.encode`, `yaml.encode`, `toml.encode`): what a document format
/// can write.
fn encodable(v: &Value) -> bool {
    !v.any_scalar(&mut |x| {
        matches!(
            x,
            Value::Ref { .. } | Value::CloudRef { .. } | Value::Null { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.len` and a refinement's `len` are one: characters, elements,
    /// fields.
    #[test]
    fn len_counts_characters_elements_and_fields() {
        assert_eq!(len(&Value::Str("héllo".into())), Some(5));
        assert_eq!(len(&Value::List(vec![Value::Int(1)])), Some(1));
        assert_eq!(len(&Value::Obj(Default::default())), Some(0));
        assert_eq!(len(&Value::Int(3)), None);
    }

    #[test]
    fn the_shipped_files_load() {
        let r = registry();
        let f = r.get("inet.subnet").unwrap();
        assert_eq!(
            f.signature,
            "inet.subnet(net: inet, bits: int, n: int) -> inet"
        );
        assert_eq!(f.file, "std/inet.df");
        assert!(!f.summary.is_empty() && f.example.contains("inet.subnet("));
        let f = r.get("str.format").unwrap();
        assert!(f.variadic && f.forwards && f.takes(3) && !f.takes(0));
        assert!(
            r.get("__path")
                .is_some_and(|f| f.internal && f.forwards && f.forwards_nulls)
        );
        assert!(callable("int.round") && !callable("int") && !callable("add"));
        assert!(callable("ref") && callable("cloud_ref") && !callable("scoped"));
        assert!(!callable(crate::address::REF) && !callable(crate::address::SCOPED));
        assert_eq!(
            r.packages(),
            [
                "base64", "csv", "hash", "inet", "int", "io", "ip", "json", "list", "oci", "path",
                "quantity", "random", "regex", "secret", "semver", "str", "time", "toml", "uri",
                "yaml"
            ]
        );
    }

    /// An object return type (R-6): a function's return shape written
    /// out in full, found past its balanced braces.
    #[test]
    fn an_object_return_type_is_one_type() {
        let f = parse(
            "t.df",
            "package t\nfn f(a: string) -> { scheme: string, port: int?, query: object }?\n",
        )
        .unwrap();
        assert_eq!(f[0].ret, "{ scheme: string, port: int?, query: object }");
        assert!(f[0].partial);
    }

    #[test]
    fn a_signature_file_is_checked() {
        let bad = |text: &str| Registry::load(&[("t.df", text)]).unwrap_err();
        assert!(bad("fn f(a: int) -> int").contains("before `package`"));
        assert!(bad("package p\nfn f(a) -> int").contains("has no type"));
        assert!(bad("package p\nfn f(a: int) -> int sometimes").contains("unknown flag"));
        assert!(bad("package p\nfn f(a: int) -> int\nfn f(b: int) -> int").contains("twice"));
        // No bare function but the lowering's own (R-155).
        assert!(bad("package lowering\nfn geo(s: string) -> string").contains("the lowering's"));
        assert!(
            Registry::load(&[(
                "t.df",
                "package lowering\ninternal fn __geo(s: string) -> string"
            )])
            .is_ok()
        );
        let two = Registry::load(&[
            ("a.df", "package geo\nfn area(a: int) -> int"),
            ("b.df", "package geo\nfn distance(a: int, b: int) -> int"),
        ]);
        assert!(two.unwrap_err().contains("also declared"));
    }

    /// Every function `std/*.df` declares has a body (DESIGN.org R-6:
    /// one registry).
    #[test]
    fn every_declared_function_has_a_body() {
        for f in registry().functions().filter(|f| !f.coeffect) {
            assert!(
                body(&f.name).is_some(),
                "{} ({}:{}) has no body",
                f.name,
                f.file,
                f.line
            );
        }
    }

    /// A signature past the width wraps its parameters, one a line, the
    /// last with its comma; it reads as the one-line form does.
    #[test]
    fn a_wrapped_signature_reads_as_one_line() {
        let wrapped = parse(
            "t.df",
            "package t\n#| A thing.\nfn f(\n  a: string,\n  b?: int,\n) -> { x: string, y: int? }?\nfn g() -> int\n",
        )
        .unwrap();
        let line = parse(
            "t.df",
            "package t\n#| A thing.\nfn f(a: string, b?: int) -> { x: string, y: int? }?\nfn g() -> int\n",
        )
        .unwrap();
        assert_eq!(wrapped[0].signature, line[0].signature);
        assert_eq!(wrapped[0].params, line[0].params);
        assert_eq!((wrapped[0].line, wrapped[1].line), (3, 7));
    }

    /// The std audit's rules, checked over every signature std/*.df
    /// declares where a rule can be read off a signature (R-134): (3),
    /// (5) and (6) below.
    ///
    /// (3) `?` is for a valid input with no answer: a partial function's
    /// summary says when it has no value, and any other function's says
    /// none (what it does not take is an error, not a none).
    ///
    /// (5) `forwards` by content: a judgment of a value (a function to
    /// `bool`, a number, a measure) does not forward a secret, it inspects
    /// it (E0301); a function whose result is its arguments' content
    /// (`min`, `sort`, `slice`, an encoder, a `with_*`) forwards. The
    /// derivations (`random.*`, `hash.sha256`) and the lowering's own
    /// forms make values of their own.
    #[test]
    fn std_follows_the_audits_rules() {
        let derives = |f: &Function| {
            // A coeffect's value is what it reads, not its arguments'.
            f.coeffect
                || f.package == "random"
                || f.name == "hash.sha256"
                || matches!(f.name.as_str(), "secret.declassify")
        };
        // (6) Subject first (`std_has_no_bare_function_and_a_package_per_subject`),
        // options last (`parse` refuses the rest), and the one variadic is
        // `str.format`'s values after its template: `path.join` takes a
        // list, as `list.join` does.
        for f in registry().functions().filter(|f| !f.internal) {
            assert!(
                !f.variadic || f.name == crate::address::FORMAT,
                "{}: a function takes a list, not any number of values ({}:{})",
                f.signature,
                f.file,
                f.line
            );
            let judgment = matches!(f.ret.as_str(), "bool" | "int" | "float" | "number");
            if judgment {
                assert!(
                    !f.forwards,
                    "{}: a judgment of a value does not forward it ({}:{})",
                    f.signature, f.file, f.line
                );
            } else if !derives(f) {
                assert!(
                    f.forwards,
                    "{}: a function of its arguments' content forwards ({}:{})",
                    f.signature, f.file, f.line
                );
            }
            let says = f.summary.contains("no value");
            assert_eq!(
                f.partial, says,
                "{}: `?` is for a valid input with no answer, said in the summary as \
                 \"no value\" ({}:{})",
                f.signature, f.file, f.line
            );
        }
    }

    /// The std audit's rules of R-155: no bare function a program calls
    /// (and a bare name of one resolves to none); every function in a
    /// package named by its subject's type (or the format of the text a
    /// decoder reads), its subject first; each function `pure` (a body)
    /// or a `coeffect` (a read, no body), `io.read` and `oci.resolve` the coeffects of
    /// std, `random.*` pure given their key.
    #[test]
    fn std_has_no_bare_function_and_a_package_per_subject() {
        // The types a package's functions take first.
        let subject = |f: &Function| -> &[&str] {
            match f.package.as_str() {
                "str" | "path" | "regex" | "hash" | "base64" | "random" | "io" => &["string"],
                "inet" => &["inet"],
                "ip" => &["ip"],
                "time" => &["time"],
                "oci" => &["oci"],
                "uri" => &["uri"],
                "semver" => &["semver"],
                "list" => &["list"],
                // An int's functions; an int from a float is about the int
                // it makes (`int.round`).
                "int" => &["int", "float"],
                // A format's: a decode takes the text, an encode the value.
                "json" | "yaml" | "toml" | "csv" => &["string", "any", "list"],
                // A quantity is a bytes, a cpu or a duration; a secret any.
                "quantity" | "secret" => &["any"],
                p => panic!("{}: the package {p} names no subject's type", f.signature),
            }
        };
        let r = registry();
        for f in r.functions().filter(|f| !f.internal) {
            assert!(
                f.name.contains('.'),
                "{}: no function is bare (R-155) ({}:{})",
                f.signature,
                f.file,
                f.line
            );
            // Its subject, or a list of them (`path.join`); a list's
            // functions take any list.
            let first = f.params.first().map(|p| p.ty.as_str()).unwrap_or("");
            let of = |t: &str| {
                first == t
                    || first
                        .strip_prefix("list(")
                        .and_then(|r| r.strip_suffix(')'))
                        == Some(t)
                    || (t == "list" && first.starts_with("list("))
            };
            // A component's escape is about the text going into one.
            if f.name != "uri.escape" {
                assert!(
                    subject(f).iter().any(|t| of(t)),
                    "{}: the package {} takes its subject first, one of {:?} ({}:{})",
                    f.signature,
                    f.package,
                    subject(f),
                    f.file,
                    f.line
                );
            }
            match kind(f) {
                "pure" => assert!(body(&f.name).is_some(), "{}: pure, with a body", f.name),
                _ => assert!(
                    body(&f.name).is_none(),
                    "{}: a coeffect has no body",
                    f.name
                ),
            }
            // A bare name of it is no function (`split`, `read`); an
            // aggregate's (`min`, `sum`) is bound in a body.
            let bare = f.name.rsplit('.').next().unwrap_or_default();
            if !crate::partition::AGGREGATES.contains(&bare) {
                let src = format!("p(x) where x = {bare}(\"a\")\n");
                let e = crate::parser::parse_program(&src).unwrap_err().to_string();
                assert!(
                    e.contains(&format!("unknown function {bare}")),
                    "{bare}: {e}"
                );
            }
        }
        let coeffects: Vec<&str> = r
            .functions()
            .filter(|f| f.coeffect)
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(coeffects, ["io.read", "oci.resolve"]);
        assert!(
            r.functions()
                .filter(|f| f.package == "random")
                .all(|f| kind(f) == "pure")
        );
        // A third kind is no flag a signature file takes.
        let e = Registry::load(&[("t.df", "package t\nfn f(a: int) -> int writes")]).unwrap_err();
        assert!(e.contains("unknown flag `writes`"), "{e}");
    }

    /// docs/grammar.md's operator table is [`OPERATORS`]: each row's
    /// operator and the types it names, outside its parentheses.
    #[test]
    fn the_operator_table_is_the_grammars() {
        let grammar = include_str!("../../../docs/grammar.md");
        let start = grammar
            .find("| operator | types |")
            .expect("the operator table");
        let mut rows = Vec::new();
        for line in grammar[start..].lines().skip(2) {
            let Some(line) = line.strip_prefix('|') else {
                break;
            };
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let op = cells[0].trim_matches('`').to_string();
            // The types: backquoted words, a parenthesis aside.
            let mut text = String::new();
            let mut depth = 0;
            for c in cells[1].chars() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    c if depth == 0 => text.push(c),
                    _ => {}
                }
            }
            let mut types: Vec<String> = text
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase()))
                .map(String::from)
                .collect();
            types.sort();
            types.dedup();
            rows.push((op, types));
        }
        let mut want: Vec<(String, Vec<String>)> = OPERATORS
            .iter()
            .map(|(op, ts)| {
                let mut ts: Vec<String> = ts.iter().map(|t| t.to_string()).collect();
                ts.sort();
                (op.to_string(), ts)
            })
            .collect();
        want.push(("==".to_string(), Vec::new()));
        rows.sort();
        want.sort();
        assert_eq!(rows, want);
    }

    /// The reverse: every body is declared somewhere (no orphan).
    #[test]
    fn every_body_is_declared() {
        let r = registry();
        for (name, _) in BODIES {
            assert!(
                r.get(name).is_some(),
                "the body {name} is declared in no std/*.df"
            );
        }
    }
}

// random

/// `random.*` (std/random.df, R-60): derived, not drawn. Each value is
/// HKDF-SHA256 of the deployment's master secret, its info the function,
/// the deployment, the key and every knob, so it is the same on every run
/// and a changed knob or master is a new value. The master is the run's
/// (`custody::Master`): a deployment's evaluation sets it on its thread; with
/// none (an editor, a bare evaluation) the functions have no value.
pub mod random {
    use crate::value::Value;
    use std::cell::RefCell;

    /// A thread's masters: the deployment's (`None` in a run that does not
    /// hold it) and the public one its stand-ins derive from
    /// (`secrets::standin`).
    struct Masters {
        real: Option<Vec<u8>>,
        standin: Vec<u8>,
        deployment: String,
        /// The epoch `real` is (R-165), and the earlier ones a secret
        /// still derives from: each epoch's master and stand-in master.
        epoch: u32,
        earlier: std::collections::BTreeMap<u32, (Option<Vec<u8>>, Vec<u8>)>,
    }

    /// The public master a stand-in derives from, made from a master id.
    fn standin_of(id: Option<&str>) -> Vec<u8> {
        use sha2::Digest;
        sha2::Sha256::new()
            .chain_update(b"dform stand-in\0")
            .chain_update(id.unwrap_or_default().as_bytes())
            .finalize()
            .to_vec()
    }

    thread_local! {
        static MASTER: RefCell<Option<Masters>> = const { RefCell::new(None) };
        /// `derive` derives a stand-in.
        static STANDIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The secrets this thread derived since its master was set, each
        /// by the call that derived it (`random.password("db")`): what the
        /// plan, `query` and `why` label one with.
        static DERIVED: RefCell<std::collections::BTreeMap<Value, String>> =
            const { RefCell::new(std::collections::BTreeMap::new()) };
        /// The deployment's rotations, by key (R-161, `State::secrets`).
        static RECORDS: RefCell<std::collections::BTreeMap<String, crate::state::Secret>> =
            const { RefCell::new(std::collections::BTreeMap::new()) };
        /// When the deployment's master was first applied (its log's
        /// `master` entry): a key never rotated is as old (R-161).
        static BORN: RefCell<Option<String>> = const { RefCell::new(None) };
        /// The generation `derive` derives at: the call's.
        static GENERATION: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
        /// The epoch `derive` derives from (R-165): the key's; 0 the
        /// current one.
        static EPOCH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        /// Each key a `random.*` call derived for since the rotations were
        /// set, with its functions and whether one is a secret: what
        /// `dform secrets list` lists.
        static CALLS: RefCell<std::collections::BTreeMap<String, Called>> =
            const { RefCell::new(std::collections::BTreeMap::new()) };
    }

    /// What a rotated secret's label says after its call: `generation N,
    /// rotated DAY by WHO` follows it (R-161), and the plan prints it at
    /// every level.
    pub const ROTATED: &str = "generation ";

    /// What a run's calls derived for one key.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Called {
        /// `password`, `signing_key`, ...
        pub functions: std::collections::BTreeSet<String>,
        /// One of them is a secret (`password`, `base64`, `signing_key`).
        pub secret: bool,
        /// The secret values derived (a stand-in's, in a run without the
        /// master): where they reach is where the key is read.
        pub values: std::collections::BTreeSet<Value>,
    }

    /// Derive this thread's `random.*` with the deployment's rotations
    /// (R-161): each key at its generation, 1 for one never rotated.
    pub fn set_secrets(records: &std::collections::BTreeMap<String, crate::state::Secret>) {
        RECORDS.with(|r| *r.borrow_mut() = records.clone());
        CALLS.with(|c| c.borrow_mut().clear());
    }

    /// When the deployment's master was first applied, from its log: the
    /// age of a key never rotated (`secrets/4`).
    pub fn set_born(born: Option<String>) {
        BORN.with(|b| *b.borrow_mut() = born);
    }

    pub fn born() -> Option<String> {
        BORN.with(|b| b.borrow().clone())
    }

    /// The keys this thread's calls derived for since the rotations were
    /// set.
    pub fn calls() -> std::collections::BTreeMap<String, Called> {
        CALLS.with(|c| c.borrow().clone())
    }

    /// Each secret key whose derived value `text` holds, at its current
    /// generation: what an attribute is made with (R-198).
    pub fn generations_in(text: &str) -> std::collections::BTreeMap<String, u32> {
        CALLS.with(|c| {
            c.borrow()
                .iter()
                .filter(|(_, c)| c.secret)
                .filter(|(_, c)| {
                    c.values
                        .iter()
                        .any(|v| matches!(v, Value::Str(x) if !x.is_empty() && text.contains(x.as_str())))
                })
                .map(|(k, _)| (k.clone(), generation_of(k)))
                .collect()
        })
    }

    /// `key`'s current generation: 1 unless rotated.
    fn generation_of(key: &str) -> u32 {
        RECORDS.with(|r| r.borrow().get(key).map_or(1, |s| s.generation))
    }

    /// `random.generation(key)`: the key's current generation, public.
    pub fn generation(a: &[Value]) -> Option<Value> {
        let [Value::Str(key)] = a else { return None };
        Some(Value::Int(generation_of(key).into()))
    }

    /// Derive this thread's `random.*` for `deployment` until the next
    /// call: from `ikm`, the master's, and each value's stand-in from a
    /// public master the master id `id` makes; with no `ikm` (a run that
    /// does not hold the master) the stand-in is the value.
    pub fn set_master(ikm: Option<Vec<u8>>, id: Option<&str>, deployment: &str) {
        MASTER.with(|m| {
            *m.borrow_mut() = Some(Masters {
                real: ikm,
                standin: standin_of(id),
                deployment: deployment.to_string(),
                epoch: 1,
                earlier: Default::default(),
            })
        });
        DERIVED.with(|d| d.borrow_mut().clear());
    }

    /// The deployment's masters by epoch (R-165, `custody::Master::epochs`):
    /// each its id and, when the run holds it, its input key material. The
    /// last is the current one, which `set_master` set; a key derives from
    /// its record's epoch, the current one unless pinned.
    pub fn set_epochs(epochs: &[(u32, String, Option<Vec<u8>>)]) {
        MASTER.with(|m| {
            let mut m = m.borrow_mut();
            let Some(m) = m.as_mut() else { return };
            let Some(((current, ..), earlier)) = epochs.split_last() else {
                return;
            };
            m.epoch = *current;
            m.earlier = earlier
                .iter()
                .map(|(e, id, ikm)| (*e, (ikm.clone(), standin_of(Some(id)))))
                .collect();
        });
    }

    /// The secrets derived on this thread since its master was set, each
    /// with its label.
    pub fn derived() -> Vec<(Value, String)> {
        DERIVED.with(|d| {
            d.borrow()
                .iter()
                .map(|(v, l)| (v.clone(), l.clone()))
                .collect()
        })
    }

    /// The value `f` derives for `random.WHAT(key, ..)` at the generation
    /// `asked` (the key's current one when `None`), its stand-in
    /// registered (`secrets::standin`), and, a `secret` one, labelled: a
    /// rotated one with its reason (`random.password("db"), generation 2,
    /// rotated 2026-10-07 by simon`). A generation past the current one is
    /// no input it takes.
    fn derived_value(
        what: &str,
        key: &str,
        asked: Option<i64>,
        secret: bool,
        f: impl Fn() -> Option<Value>,
    ) -> Option<Value> {
        let current = generation_of(key);
        let generation = match asked {
            None => current,
            Some(g) => u32::try_from(g)
                .ok()
                .filter(|g| (1..=current).contains(g))?,
        };
        let epoch = RECORDS.with(|r| r.borrow().get(key).and_then(|s| s.epoch).unwrap_or(0));
        let at = |standin: bool| {
            GENERATION.with(|g| g.set(generation));
            EPOCH.with(|e| e.set(epoch));
            STANDIN.with(|s| s.set(standin));
            let v = f();
            STANDIN.with(|s| s.set(false));
            EPOCH.with(|e| e.set(0));
            GENERATION.with(|g| g.set(1));
            v
        };
        let standin = at(true)?;
        let real = MASTER.with(|m| m.borrow().as_ref().map(|m| m.real.is_some()))?;
        let v = match real {
            true => at(false)?,
            false => standin.clone(),
        };
        CALLS.with(|c| {
            let mut c = c.borrow_mut();
            let e = c.entry(key.to_string()).or_default();
            e.functions.insert(what.to_string());
            e.secret |= secret;
            if secret {
                e.values.insert(v.clone());
            }
        });
        let record = RECORDS.with(|r| r.borrow().get(key).cloned());
        let label = match (asked, record) {
            (Some(_), _) => format!("random.{what}({key:?}, generation: {generation})"),
            (None, Some(r)) if r.generation > 1 => format!(
                "random.{what}({key:?}), {ROTATED}{generation}, rotated {} by {}",
                r.day(),
                r.by
            ),
            (None, _) => format!("random.{what}({key:?})"),
        };
        if let (Value::Str(v), Value::Str(s)) = (&v, &standin) {
            crate::secrets::standin::register(v, &label, s);
        }
        if secret {
            DERIVED.with(|d| {
                d.borrow_mut().entry(v.clone()).or_insert(label);
            });
        }
        Some(v)
    }

    /// Whether `program` calls a `random.*` function: its run needs a master.
    pub fn called(program: &crate::ast::Program) -> bool {
        fn term(t: &crate::ast::Term) -> bool {
            use crate::ast::Term;
            match t {
                Term::Func { name, args } => name.starts_with("random.") || args.iter().any(term),
                Term::List(xs) => xs.iter().any(term),
                Term::Obj(m) => m.values().any(term),
                _ => false,
            }
        }
        fn atom(a: &crate::ast::Atom) -> bool {
            a.args.iter().any(term)
        }
        use crate::ast::{Lit, Stmt};
        program.statements.iter().any(|s| match s {
            Stmt::Fact(a) => atom(a),
            Stmt::Rule(r) => {
                atom(&r.head)
                    || r.body.iter().any(|l| match l {
                        Lit::Pos(a) | Lit::Not(a) => atom(a),
                        Lit::Eq(x, y)
                        | Lit::Neq(x, y)
                        | Lit::Gt(x, y)
                        | Lit::Ge(x, y)
                        | Lit::Lt(x, y)
                        | Lit::Le(x, y) => term(x) || term(y),
                    })
            }
            _ => false,
        })
    }

    /// `len` bytes for the call `what` of `key` with `knobs`.
    fn derive(what: &str, key: &str, knobs: &[&str], len: usize) -> Option<Vec<u8>> {
        MASTER.with(|m| {
            let m = m.borrow();
            let m = m.as_ref()?;
            // The key's epoch's master (R-165): the current one unless the
            // key is pinned to an earlier one.
            let (real, standin) = match EPOCH.with(|e| e.get()) {
                0 => (m.real.as_ref(), &m.standin),
                e if e == m.epoch => (m.real.as_ref(), &m.standin),
                e => {
                    let (real, standin) = m.earlier.get(&e)?;
                    (real.as_ref(), standin)
                }
            };
            let ikm = match STANDIN.with(|s| s.get()) {
                true => standin,
                false => real?,
            };
            let deployment = &m.deployment;
            let mut info = Vec::new();
            for part in [what, deployment.as_str(), key].iter().chain(knobs) {
                info.extend_from_slice(part.as_bytes());
                info.push(0);
            }
            // A rotated key's generation (R-161); generation 1 adds
            // nothing, so a key never rotated derives what it always did.
            let generation = GENERATION.with(|g| g.get());
            if generation > 1 {
                info.extend_from_slice(format!("generation\0{generation}\0").as_bytes());
            }
            Some(crate::secrets::hkdf(b"dform random", ikm, &info, len))
        })
    }

    /// `n` characters of `alphabet`, uniform: bytes past the largest
    /// multiple of its size are skipped.
    fn chars(what: &str, key: &str, knobs: &[&str], n: usize, alphabet: &[u8]) -> Option<String> {
        let limit = 256 - 256 % alphabet.len();
        let mut out = String::with_capacity(n);
        // Twice what is needed, and more rounds in the very unlikely case
        // that is not enough.
        for round in 0.. {
            let r = round.to_string();
            let mut ks: Vec<&str> = knobs.to_vec();
            ks.push(&r);
            let bytes = derive(what, key, &ks, (2 * n + 32).min(255 * 32))?;
            for b in bytes {
                if (b as usize) < limit {
                    out.push(alphabet[b as usize % alphabet.len()] as char);
                    if out.len() == n {
                        return Some(out);
                    }
                }
            }
        }
        None
    }

    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    const BASE64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn alphabet(name: &str) -> Option<Vec<u8>> {
        Some(match name {
            "alnum" => ALNUM.to_vec(),
            "ascii" => (b'!'..=b'~').collect(),
            "hex" => b"0123456789abcdef".to_vec(),
            "base64" => BASE64.to_vec(),
            _ => return None,
        })
    }

    /// A call's arguments and its `generation` (R-161), the parameter
    /// after the `n` others: none when it is left out.
    fn generation_arg(a: &[Value], n: usize) -> Option<(&[Value], Option<i64>)> {
        match a.len() {
            l if l <= n => Some((a, None)),
            l if l == n + 1 => match &a[n] {
                Value::Int(g) => Some((&a[..n], Some(*g))),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn password(a: &[Value]) -> Option<Value> {
        let (a, generation) = generation_arg(a, 3)?;
        let (key, length, name) = match a {
            [Value::Str(k)] => (k, 32, "alnum"),
            [Value::Str(k), Value::Int(n)] => (k, *n, "alnum"),
            [Value::Str(k), Value::Int(n), Value::Str(al)] => (k, *n, al.as_str()),
            _ => return None,
        };
        if !(1..=1024).contains(&length) {
            return None;
        }
        let set = alphabet(name)?;
        let n = length.to_string();
        derived_value("password", key, generation, true, || {
            chars("password", key, &[&n, name], length as usize, &set).map(Value::Str)
        })
    }

    /// `random.base64`: derived as `random.bytes` was (its derivation's
    /// name kept, so a rename is no rotation).
    pub fn base64(a: &[Value]) -> Option<Value> {
        use base64::Engine;
        let (a, generation) = generation_arg(a, 2)?;
        let [Value::Str(key), Value::Int(n)] = a else {
            return None;
        };
        if !(1..=4096).contains(n) {
            return None;
        }
        derived_value("base64", key, generation, true, || {
            let b = derive("bytes", key, &[&n.to_string()], *n as usize)?;
            Some(Value::Str(
                base64::engine::general_purpose::STANDARD.encode(b),
            ))
        })
    }

    pub fn id(a: &[Value]) -> Option<Value> {
        let (a, generation) = generation_arg(a, 2)?;
        let (key, n) = match a {
            [Value::Str(k)] => (k, 8),
            [Value::Str(k), Value::Int(n)] => (k, *n),
            _ => return None,
        };
        if !(1..=64).contains(&n) {
            return None;
        }
        derived_value("id", key, generation, false, || {
            let b = derive("id", key, &[&n.to_string()], n as usize)?;
            Some(Value::Str(b.iter().map(|x| format!("{x:02x}")).collect()))
        })
    }

    pub fn uuid(a: &[Value]) -> Option<Value> {
        let (a, generation) = generation_arg(a, 1)?;
        let [Value::Str(key)] = a else { return None };
        derived_value("uuid", key, generation, false, || {
            let mut b = derive("uuid", key, &[], 16)?;
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
            Some(Value::Str(format!(
                "{}-{}-{}-{}-{}",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            )))
        })
    }

    /// The version and the seed of `key`'s ed25519 signing key, at the
    /// generation `derive` derives at.
    fn ed25519(key: &str) -> Option<(String, Vec<u8>)> {
        let version = chars("signing_key version", key, &[], 4, &ALNUM[..52])?;
        let seed = derive("signing_key ed25519", key, &[], 32)?;
        Some((version, seed))
    }

    /// Synapse's signing key file: `ed25519 a_XXXX SEED`, the version four
    /// letters and the 32-byte seed unpadded base64 (what
    /// `generate_signing_key` writes).
    pub fn signing_key(a: &[Value]) -> Option<Value> {
        use base64::Engine;
        let (a, generation) = generation_arg(a, 1)?;
        let [Value::Str(key)] = a else { return None };
        derived_value("signing_key", key, generation, true, || {
            let (version, seed) = ed25519(key)?;
            Some(Value::Str(format!(
                "ed25519 a_{version} {}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(seed)
            )))
        })
    }

    /// The public half of `random.signing_key(key)` (R-161): `ed25519
    /// a_XXXX KEY`, the verify key unpadded base64, as Synapse publishes
    /// one (`old_signing_keys`). Public: it is what remote servers verify
    /// with.
    pub fn verify_key(a: &[Value]) -> Option<Value> {
        use base64::Engine;
        let (a, generation) = generation_arg(a, 1)?;
        let [Value::Str(key)] = a else { return None };
        derived_value("verify_key", key, generation, false, || {
            let (version, seed) = ed25519(key)?;
            let seed: [u8; 32] = seed.try_into().ok()?;
            let public = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
            Some(Value::Str(format!(
                "ed25519 a_{version} {}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(public.as_bytes())
            )))
        })
    }
}

#[cfg(test)]
mod random_tests {
    use super::random::*;
    use crate::value::Value;

    fn s(v: Option<Value>) -> String {
        match v {
            Some(Value::Str(s)) => s,
            v => panic!("{v:?}"),
        }
    }

    /// Derived: the same for the same master, deployment, key and knobs;
    /// another for any of them changed; none without a master.
    #[test]
    fn a_value_is_derived_from_the_master_and_every_knob() {
        let k = |x: &str| Value::Str(x.into());
        set_master(Some(b"m1".to_vec()), None, "app");
        let pw = s(password(&[k("db")]));
        assert!(pw.len() == 32 && pw.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(pw, s(password(&[k("db")])));
        assert_eq!(pw, s(password(&[k("db"), Value::Int(32), k("alnum")])));
        assert_ne!(pw, s(password(&[k("other")])));
        let long = s(password(&[k("db"), Value::Int(40)]));
        assert_eq!(long.len(), 40);
        assert!(!long.starts_with(&pw), "a length is in the derivation");
        let hex = s(password(&[k("db"), Value::Int(16), k("hex")]));
        assert!(hex.len() == 16 && hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(password(&[k("db"), Value::Int(16), k("emoji")]).is_none());
        assert!(password(&[k("db"), Value::Int(0)]).is_none());
        let b = s(base64(&[k("cookie"), Value::Int(32)]));
        assert_eq!(b.len(), 44);
        let id = s(id(&[k("logs")]));
        assert_eq!(id.len(), 16);
        let u = s(uuid(&[k("tenant")]));
        assert!(u.len() == 36 && u.as_bytes()[14] == b'4', "{u}");
        let sk = s(signing_key(&[k("synapse")]));
        let parts: Vec<&str> = sk.split(' ').collect();
        assert!(
            parts.len() == 3
                && parts[0] == "ed25519"
                && parts[1].len() == 6
                && parts[1].starts_with("a_")
                && parts[2].len() == 43,
            "{sk}"
        );
        set_master(Some(b"m1".to_vec()), None, "app[env=prod]");
        assert_ne!(pw, s(password(&[k("db")])), "the deployment is in it");
        set_master(Some(b"m2".to_vec()), None, "app");
        assert_ne!(pw, s(password(&[k("db")])), "the master is in it");
    }

    /// A run that does not hold the master derives each value's stand-in:
    /// the same shape, a function of the master id and the derivation's
    /// inputs, never the value; a run that holds it derives the same
    /// stand-in beside the value.
    #[test]
    fn without_the_master_a_value_is_its_stand_in() {
        let k = |x: &str| Value::Str(x.into());
        set_master(Some(b"m1".to_vec()), Some("id-1"), "app");
        let pw = s(password(&[k("db")]));
        set_master(None, Some("id-1"), "app");
        let standin = s(password(&[k("db")]));
        assert_ne!(pw, standin);
        assert!(standin.len() == 32 && standin.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(standin, s(password(&[k("db")])), "stable");
        set_master(None, Some("id-2"), "app");
        assert_ne!(standin, s(password(&[k("db")])), "the master id is in it");
        set_master(None, Some("id-1"), "app");
        assert_ne!(standin, s(password(&[k("db-2")])), "the key is in it");
    }

    /// A key's generation (R-161) is one more input: generation 1 is the
    /// value a key never rotated always had, each later one another, and
    /// an earlier one is asked for by name; a generation past the current
    /// one is none.
    #[test]
    fn a_generation_is_in_the_derivation() {
        let k = |x: &str| Value::Str(x.into());
        set_master(Some(b"m1".to_vec()), None, "app");
        set_secrets(&Default::default());
        let first = s(password(&[k("db")]));
        let other = s(password(&[k("other")]));
        let rotated = crate::state::Secret {
            generation: 2,
            epoch: None,
            rotated_at: "2026-10-07T09:00:00Z".into(),
            by: "alice".into(),
            pending: true,
        };
        set_secrets(&[("db".to_string(), rotated)].into_iter().collect());
        let second = s(password(&[k("db")]));
        assert_ne!(first, second);
        assert_eq!(
            other,
            s(password(&[k("other")])),
            "another key is as it was"
        );
        let at = |g| password(&[k("db"), Value::Int(32), k("alnum"), Value::Int(g)]);
        assert_eq!(first, s(at(1)));
        assert_eq!(second, s(at(2)));
        assert!(at(3).is_none() && at(0).is_none());
        assert!(
            derived()
                .iter()
                .any(|(_, l)| l
                    == "random.password(\"db\"), generation 2, rotated 2026-10-07 by alice"),
            "{:?}",
            derived()
        );
        assert_eq!(generation(&[k("db")]), Some(Value::Int(2)));
        assert_eq!(generation(&[k("other")]), Some(Value::Int(1)));
    }

    /// `random.verify_key` is the public half of `random.signing_key`, at
    /// the same generation.
    #[test]
    fn a_verify_key_is_its_signing_keys_public_half() {
        use base64::Engine;
        let k = |x: &str| Value::Str(x.into());
        set_master(Some(b"m1".to_vec()), None, "app");
        set_secrets(&Default::default());
        let signing = s(signing_key(&[k("synapse")]));
        let verify = s(verify_key(&[k("synapse")]));
        let (sv, seed) = signing.rsplit_once(' ').unwrap();
        let (vv, public) = verify.rsplit_once(' ').unwrap();
        assert_eq!(sv, vv, "the same version");
        let seed: [u8; 32] = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(seed)
            .unwrap()
            .try_into()
            .unwrap();
        let want = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        assert_eq!(
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(want.as_bytes()),
            public
        );
        assert_eq!(verify, s(verify_key(&[k("synapse"), Value::Int(1)])));
    }

    #[test]
    fn with_no_master_there_is_no_value() {
        assert!(password(&[Value::Str("db".into())]).is_none());
    }
}

#[cfg(test)]
mod named_tests {
    use crate::ast::Term;
    use crate::value::Value;

    fn v(x: Value) -> Term {
        Term::Val(x)
    }

    /// A named argument goes where its parameter is; one left out before
    /// it takes its default, or is an error naming it.
    #[test]
    fn a_named_argument_takes_its_parameters_place() {
        let f = super::get("random.password").unwrap();
        let key = v(Value::Str("db".into()));
        let got = super::with_named(
            f,
            vec![key.clone()],
            vec![("generation".into(), v(Value::Int(1)))],
        )
        .unwrap();
        assert_eq!(
            got,
            vec![
                key.clone(),
                v(Value::Int(32)),
                v(Value::Str("alnum".into())),
                v(Value::Int(1))
            ]
        );
        let e = super::with_named(
            f,
            vec![key.clone()],
            vec![("nope".into(), v(Value::Int(1)))],
        )
        .unwrap_err();
        assert!(
            e.contains("`random.password` has no parameter `nope`"),
            "{e}"
        );
        let e = super::with_named(
            f,
            vec![key.clone(), v(Value::Int(8))],
            vec![("length".into(), v(Value::Int(1)))],
        )
        .unwrap_err();
        assert!(e.contains("`length` is given twice"), "{e}");
        let b = super::get("random.base64").unwrap();
        let e = super::with_named(b, vec![key], vec![("generation".into(), v(Value::Int(1)))])
            .unwrap_err();
        assert!(e.contains("`random.base64`'s `length` is left out"), "{e}");
    }
}
