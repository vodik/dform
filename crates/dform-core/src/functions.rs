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
//! (`inet.subnet`); the prelude's are bare (`int(s)`, `format`). `?` after
//! the result marks a partial function, `forwards` one a secret flows
//! through uninspected (`secrets`, E0301), `forwards nulls` one whose null
//! arguments are not content positions (Rule 2), and `internal` the
//! lowering's own (`add`, `__path`), which a program may not call. Purity
//! is not a flag: every function is pure, impurity enters through externs.
//! The bodies are the engine's (`engine::body`), looked up by the
//! qualified name; a test keeps the two in step.
//!
//! The format is self-contained text (DESIGN.org R-24: a function package
//! embeds its signature file), read for now by this module rather than by
//! the program parser. When R-24 embeds these files in packages, the
//! program parser takes the format over (a non-program mode) and this
//! reader goes.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::value::Value;

/// The signature files, by the path they are shipped at.
pub const SOURCES: &[(&str, &str)] = &[
    ("std/prelude.df", include_str!("../../../std/prelude.df")),
    ("std/inet.df", include_str!("../../../std/inet.df")),
    ("std/int.df", include_str!("../../../std/int.df")),
    ("std/ip.df", include_str!("../../../std/ip.df")),
    ("std/str.df", include_str!("../../../std/str.df")),
    ("std/list.df", include_str!("../../../std/list.df")),
    ("std/time.df", include_str!("../../../std/time.df")),
    ("std/duration.df", include_str!("../../../std/duration.df")),
    ("std/bytes.df", include_str!("../../../std/bytes.df")),
    ("std/cpu.df", include_str!("../../../std/cpu.df")),
    ("std/random.df", include_str!("../../../std/random.df")),
    ("std/regex.df", include_str!("../../../std/regex.df")),
    ("std/semver.df", include_str!("../../../std/semver.df")),
    ("std/oci.df", include_str!("../../../std/oci.df")),
    ("std/hash.df", include_str!("../../../std/hash.df")),
    ("std/base64.df", include_str!("../../../std/base64.df")),
    ("std/url.df", include_str!("../../../std/url.df")),
    ("std/path.df", include_str!("../../../std/path.df")),
    ("std/json.df", include_str!("../../../std/json.df")),
    ("std/yaml.df", include_str!("../../../std/yaml.df")),
    ("std/toml.df", include_str!("../../../std/toml.df")),
];

/// The package whose functions are written bare.
pub const PRELUDE: &str = "prelude";

/// Type names a prelude function may share only as that type's
/// constructor (`inet(s) -> inet`).
const TYPE_NAMES: &[&str] = &[
    "int", "string", "bool", "inet", "ip", "iprange", "list", "any", "ref", "secret", "symbol",
    "addr", "bytes", "cpu", "duration", "time", "url",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: String,
    /// `name?: T`: a call may leave it out (the last parameters only).
    pub optional: bool,
}

/// One declared function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// The name a call is written with: `inet.subnet`, or bare in the prelude.
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

    /// A function to bool is also a predicate: `inet.contains(n, a)` as a
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

/// Whether a program may call `name`: declared and not internal.
pub fn callable(name: &str) -> bool {
    get(name).is_some_and(|f| !f.internal)
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
            format!("{name} is the lowering's, not a function a program calls"),
        );
        return match op {
            Some(op) => d.with_help(format!("write `a {op} b`")),
            None => d.with_help(f.summary.clone()),
        };
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
        None => {
            let prelude: Vec<&str> = r
                .functions()
                .filter(|f| f.package == PRELUDE && !f.internal)
                .map(|f| f.name.as_str())
                .collect();
            d.with_help(format!(
                "the functions are {} and the packages {} (std/*.df)",
                prelude.join(", "),
                r.packages().join(", ")
            ))
        }
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
        for p in packages.keys() {
            if let Some(f) = by_name.get(p.as_str()).filter(|f| f.package == PRELUDE)
                && f.ret != *p
            {
                return Err(format!(
                    "{}:{}: {p} is a package and a function: only a constructor of the \
                     type {p} may share its name",
                    f.file, f.line
                ));
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

    /// The function packages, the prelude aside: the heads of qualified names.
    pub fn packages(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .by_name
            .values()
            .map(|f| f.package.as_str())
            .filter(|p| *p != PRELUDE)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Whether `head` names a function package (`inet` in `inet.subnet`).
    pub fn is_package(&self, head: &str) -> bool {
        head != PRELUDE && self.by_name.values().any(|f| f.package == head)
    }
}

/// One signature file: `package NAME`, then `fn` lines, each with the
/// `#|` lines above it as its documentation (bare lines its summary,
/// `example:` its example). `#` comments and blank lines are ignored.
pub fn parse(file: &str, text: &str) -> Result<Vec<Function>, String> {
    let mut package: Option<String> = None;
    let mut doc: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
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
            if !is_name(name) {
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
        let mut f = function(sig.trim()).map_err(err)?;
        f.internal = internal;
        f.name = if package == PRELUDE {
            f.name
        } else {
            format!("{package}.{}", f.name)
        };
        if package == PRELUDE && TYPE_NAMES.contains(&f.name.as_str()) && f.ret != f.name {
            return Err(err(format!(
                "{} is a type: a function of that name is its constructor, `-> {}`",
                f.name, f.name
            )));
        }
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
            .map(|p| {
                let q = if p.optional { "?" } else { "" };
                format!("{}{q}: {}", p.name, p.ty)
            })
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

fn is_name(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `name(p: T, ...) -> T[?] [flag, ...]`, the part after `fn `. Shared
/// with `fmt` (R-24: a signature file's own normal form).
pub(crate) fn function(sig: &str) -> Result<Function, String> {
    let open = sig.find('(').ok_or("expected `(` after the name")?;
    let name = sig[..open].trim();
    if !is_name(name) {
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
        if !is_name(n) || t.is_empty() {
            return Err(format!("`{p}` is not `name: type`"));
        }
        if !optional && params.iter().any(|p: &Param| p.optional) {
            return Err(format!(
                "`{n}` follows an optional parameter: only the last ones may be left out"
            ));
        }
        params.push(Param {
            name: n.to_string(),
            ty: t.to_string(),
            optional,
        });
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
    let (mut forwards, mut forwards_nulls) = (false, false);
    for flag in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match flag.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["forwards"] => forwards = true,
            ["forwards", "nulls"] => forwards_nulls = true,
            _ => {
                return Err(format!(
                    "unknown flag `{flag}` (`forwards`, `forwards nulls`)"
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

/// A function's body: its arguments' values to its value, or none. The
/// same shape as `engine::Body`; `engine::body` falls back to this table
/// for a name it does not itself have, so the std ticket's bodies live
/// here, beside the signatures they implement (R-6).
pub type Body = fn(&[Value]) -> Option<Value>;

/// The body of every function this module declares that `engine.rs`'s
/// own table does not (the std ticket's new packages); a test keeps
/// every declared function's body, here or there, in step.
pub fn body(name: &str) -> Option<Body> {
    BODIES.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

pub const BODIES: &[(&str, Body)] = &[
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
    ("str.contains", |a| match a {
        [Value::Str(s), Value::Str(n)] => Some(Value::Bool(s.contains(n.as_str()))),
        _ => None,
    }),
    ("str.format", |a| match a {
        [Value::Str(fmt), Value::List(args)] => {
            let mut out = String::new();
            let mut parts = fmt.split("%s");
            out.push_str(parts.next().unwrap_or(""));
            let mut args = args.iter();
            for p in parts {
                out.push_str(&scalar_text(args.next()?)?);
                out.push_str(p);
            }
            Some(Value::Str(out))
        }
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
    ("str.len", |a| match a {
        [Value::Str(s)] => Some(Value::Int(s.chars().count() as i64)),
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
            keyed.sort_by(|(a, _), (b, _)| a.cmp(b));
            Some(Value::List(keyed.into_iter().map(|(_, x)| x.clone()).collect()))
        }
        _ => None,
    }),
    ("list.unique", |a| match a {
        [Value::List(xs)] => {
            let mut seen = std::collections::HashSet::new();
            Some(Value::List(
                xs.iter().filter(|x| seen.insert((*x).clone())).cloned().collect(),
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
        [Value::List(xs)] => {
            let mut total = 0i64;
            for x in xs {
                let Value::Int(n) = x else { return None };
                total = total.checked_add(*n)?;
            }
            Some(Value::Int(total))
        }
        _ => None,
    }),
    ("list.contains", |a| match a {
        [Value::List(xs), v] => Some(Value::Bool(xs.contains(v))),
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
            regex::Regex::new(re).ok()?.replace_all(s, with.as_str()).into_owned(),
        )),
        _ => None,
    }),
    ("semver.parse", |a| match a {
        [Value::Str(s)] => {
            let v = semver::Version::parse(s).ok()?;
            let mut m = BTreeMap::new();
            m.insert("major".to_string(), Value::Int(v.major as i64));
            m.insert("minor".to_string(), Value::Int(v.minor as i64));
            m.insert("patch".to_string(), Value::Int(v.patch as i64));
            if !v.pre.is_empty() {
                m.insert("pre".to_string(), Value::Str(v.pre.to_string()));
            }
            Some(Value::Obj(m))
        }
        _ => None,
    }),
    ("semver.satisfies", |a| match a {
        [Value::Str(v), Value::Str(range)] => {
            let v = semver::Version::parse(v).ok()?;
            let req = semver::VersionReq::parse(range).ok()?;
            Some(Value::Bool(req.matches(&v)))
        }
        _ => None,
    }),
    ("semver.compare", |a| match a {
        [Value::Str(x), Value::Str(y)] => {
            let x = semver::Version::parse(x).ok()?;
            let y = semver::Version::parse(y).ok()?;
            Some(Value::Int(match x.cmp(&y) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => None,
    }),
    ("oci.parse", |a| match a {
        [Value::Str(s)] => {
            let r = oci_split(s)?;
            let mut m = BTreeMap::new();
            if let Some(reg) = r.registry {
                m.insert("registry".to_string(), Value::Str(reg));
            }
            m.insert("repository".to_string(), Value::Str(r.repository));
            if let Some(t) = r.tag {
                m.insert("tag".to_string(), Value::Str(t));
            }
            if let Some(d) = r.digest {
                m.insert("digest".to_string(), Value::Str(d));
            }
            Some(Value::Obj(m))
        }
        _ => None,
    }),
    ("oci.pinned", |a| match a {
        [Value::Str(s)] => Some(Value::Bool(
            oci_split(s).is_some_and(|r| r.digest.is_some()),
        )),
        _ => None,
    }),
    ("oci.with_digest", |a| match a {
        [Value::Str(s), Value::Str(d)] => {
            let r = oci_split(s)?;
            if !is_digest(d) {
                return None;
            }
            let mut out = String::new();
            if let Some(reg) = &r.registry {
                out.push_str(reg);
                out.push('/');
            }
            out.push_str(&r.repository);
            if let Some(t) = &r.tag {
                out.push(':');
                out.push_str(t);
            }
            out.push('@');
            out.push_str(d);
            Some(Value::Str(out))
        }
        _ => None,
    }),
    ("hash.sha256", |a| match a {
        [Value::Str(s)] => Some(Value::Str(sha256_hex(s))),
        _ => None,
    }),
    ("hash.short", |a| match a {
        [Value::Str(s), Value::Int(n)] => {
            let full = sha256_hex(s);
            let n = usize::try_from(*n).ok()?;
            (n <= full.len()).then(|| Value::Str(full[..n].to_string()))
        }
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
    ("url", |a| match a {
        [Value::Str(s)] => url::Url::parse(s).ok().map(|u| Value::Str(u.to_string())),
        _ => None,
    }),
    ("url.parse", |a| match a {
        [Value::Str(s)] => {
            let u = url::Url::parse(s).ok()?;
            let mut m = BTreeMap::new();
            m.insert("scheme".to_string(), Value::Str(u.scheme().to_string()));
            m.insert("host".to_string(), Value::Str(u.host_str()?.to_string()));
            if let Some(port) = u.port() {
                m.insert("port".to_string(), Value::Int(i64::from(port)));
            }
            m.insert("path".to_string(), Value::Str(u.path().to_string()));
            let mut q = BTreeMap::new();
            for (k, v) in u.query_pairs() {
                q.insert(k.into_owned(), Value::Str(v.into_owned()));
            }
            m.insert("query".to_string(), Value::Obj(q));
            if let Some(f) = u.fragment() {
                m.insert("fragment".to_string(), Value::Str(f.to_string()));
            }
            Some(Value::Obj(m))
        }
        _ => None,
    }),
    ("url.join", |a| match a {
        [Value::Str(x), Value::Str(y)] => Some(Value::Str(join_slash(x, y))),
        _ => None,
    }),
    ("url.with_scheme", |a| match a {
        [Value::Str(s), Value::Str(scheme)] => {
            let mut u = url::Url::parse(s).ok()?;
            u.set_scheme(scheme).ok()?;
            Some(Value::Str(u.to_string()))
        }
        _ => None,
    }),
    ("url.with_host", |a| match a {
        [Value::Str(s), Value::Str(host)] => {
            let mut u = url::Url::parse(s).ok()?;
            u.set_host(Some(host)).ok()?;
            Some(Value::Str(u.to_string()))
        }
        _ => None,
    }),
    ("url.with_port", |a| match a {
        [Value::Str(s), Value::Int(port)] => {
            let mut u = url::Url::parse(s).ok()?;
            let p = u16::try_from(*port).ok()?;
            u.set_port(Some(p)).ok()?;
            Some(Value::Str(u.to_string()))
        }
        _ => None,
    }),
    ("url.with_path", |a| match a {
        [Value::Str(s), Value::Str(path)] => {
            let mut u = url::Url::parse(s).ok()?;
            u.set_path(path);
            Some(Value::Str(u.to_string()))
        }
        _ => None,
    }),
    ("url.with_query", |a| match a {
        [Value::Str(s), Value::Obj(q)] => {
            let mut u = url::Url::parse(s).ok()?;
            let pairs: Option<Vec<(&String, &String)>> = q
                .iter()
                .map(|(k, v)| match v {
                    Value::Str(s) => Some((k, s)),
                    _ => None,
                })
                .collect();
            let pairs = pairs?;
            if pairs.is_empty() {
                u.set_query(None);
            } else {
                let mut qs = url::form_urlencoded::Serializer::new(String::new());
                for (k, v) in pairs {
                    qs.append_pair(k, v);
                }
                u.set_query(Some(&qs.finish()));
            }
            Some(Value::Str(u.to_string()))
        }
        _ => None,
    }),
    ("url.encode", |a| match a {
        [Value::Str(s)] => Some(Value::Str(
            percent_encoding::utf8_percent_encode(s, URL_COMPONENT).to_string(),
        )),
        _ => None,
    }),
    ("path.join", |a| {
        let mut parts = a.iter();
        let Value::Str(first) = parts.next()? else {
            return None;
        };
        let mut out = first.clone();
        for v in parts {
            let Value::Str(s) = v else { return None };
            out = join_slash(&out, s);
        }
        Some(Value::Str(out))
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
        [Value::Str(from), Value::Str(to)] => rel_path(from, to).map(Value::Str),
        _ => None,
    }),
    ("path.clean", |a| match a {
        [Value::Str(p)] => Some(Value::Str(clean_path(p))),
        _ => None,
    }),
    ("json.decode", |a| match a {
        [Value::Str(s)] => serde_json::from_str::<serde_json::Value>(s)
            .ok()
            .and_then(|j| json_to_value(&j)),
        _ => None,
    }),
    ("json.encode", |a| match a {
        [v] if encodable(v) => serde_json::to_string(&crate::engine::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
    ("yaml.decode", |a| match a {
        [Value::Str(s)] => serde_yaml::from_str::<serde_json::Value>(s)
            .ok()
            .and_then(|j| json_to_value(&j)),
        _ => None,
    }),
    ("yaml.encode", |a| match a {
        [v] if encodable(v) => serde_yaml::to_string(&crate::engine::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
    ("toml.decode", |a| match a {
        [Value::Str(s)] => toml::from_str::<serde_json::Value>(s)
            .ok()
            .and_then(|j| json_to_value(&j)),
        _ => None,
    }),
    ("toml.encode", |a| match a {
        [v] if encodable(v) => toml::to_string(&crate::engine::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
];

/// A scalar's text (`str.format`'s args): a list, an object, a
/// reference and a null have none.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Quantity(_) | Value::Time(_) => v.typed_text(),
        Value::Ip(_) | Value::IpNet { .. } | Value::IpRange { .. } => {
            Some(crate::partition::fmt_value(v))
        }
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

/// A URL path or query component's safe characters: alphanumerics and
/// the unreserved punctuation (RFC 3986), everything else percent-encoded.
static URL_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

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
    let out = if abs {
        format!("/{joined}")
    } else {
        joined
    };
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

/// A parsed OCI distribution reference, `[registry/]repository[:tag][@digest]`.
struct OciRef {
    registry: Option<String>,
    repository: String,
    tag: Option<String>,
    digest: Option<String>,
}

fn oci_split(reference: &str) -> Option<OciRef> {
    let (rest, digest) = match reference.split_once('@') {
        Some((a, b)) if is_digest(b) => (a, Some(b.to_string())),
        Some(_) => return None,
        None => (reference, None),
    };
    if rest.is_empty() {
        return None;
    }
    let last_slash = rest.rfind('/');
    let (name, tag) = match rest.rfind(':') {
        Some(ci) if last_slash.is_none_or(|si| ci > si) => {
            let tag = &rest[ci + 1..];
            if !is_tag(tag) {
                return None;
            }
            (&rest[..ci], Some(tag.to_string()))
        }
        _ => (rest, None),
    };
    if name.is_empty() {
        return None;
    }
    let (registry, repository) = match name.split_once('/') {
        Some((head, tail)) if is_registry(head) && !tail.is_empty() => {
            (Some(head.to_string()), tail.to_string())
        }
        _ => (None, name.to_string()),
    };
    if repository.is_empty() {
        return None;
    }
    Some(OciRef {
        registry,
        repository,
        tag,
        digest,
    })
}

fn is_digest(s: &str) -> bool {
    matches!(s.split_once(':'), Some((algo, hex)) if !algo.is_empty() && !hex.is_empty()
        && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_tag(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn is_registry(s: &str) -> bool {
    !s.is_empty() && (s.contains('.') || s.contains(':') || s == "localhost")
}

/// The text's SHA-256, lower-case hex (`hash.sha256`, `hash.short`).
fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A JSON value read into a dform one (`json.decode`, `yaml.decode`,
/// `toml.decode`, all through `serde_json::Value` as their common
/// model); none for a JSON `null` (dform has no scalar for it) or a
/// number that is not whole.
fn json_to_value(j: &serde_json::Value) -> Option<Value> {
    use serde_json::Value as J;
    Some(match j {
        J::Null => return None,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => Value::Int(n.as_i64()?),
        J::String(s) => Value::Str(s.clone()),
        J::Array(xs) => Value::List(xs.iter().map(json_to_value).collect::<Option<Vec<_>>>()?),
        J::Object(m) => Value::Obj(
            m.iter()
                .map(|(k, v)| json_to_value(v).map(|v| (k.clone(), v)))
                .collect::<Option<BTreeMap<_, _>>>()?,
        ),
    })
}

/// Whether `v` has no reference and no null anywhere inside it
/// (`json.encode`, `yaml.encode`, `toml.encode`): what a document format
/// can write.
fn encodable(v: &Value) -> bool {
    match v {
        Value::Ref { .. } | Value::CloudRef { .. } | Value::Null { .. } => false,
        Value::List(xs) => xs.iter().all(encodable),
        Value::Obj(m) => m.values().all(encodable),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_files_load() {
        let r = registry();
        let f = r.get("inet.subnet").unwrap();
        assert_eq!(
            f.signature,
            "inet.subnet(net: inet, bits: int, n: int) -> inet?"
        );
        assert_eq!(f.file, "std/inet.df");
        assert!(!f.summary.is_empty() && f.example.contains("inet.subnet("));
        let f = r.get("format").unwrap();
        assert!(f.variadic && f.forwards && f.takes(3) && !f.takes(0));
        assert!(
            r.get("__path")
                .is_some_and(|f| f.internal && f.forwards && f.forwards_nulls)
        );
        assert!(callable("int") && !callable("add") && !callable("to_int"));
        assert_eq!(
            r.packages(),
            [
                "base64", "bytes", "cpu", "duration", "hash", "inet", "int", "ip", "json", "list",
                "oci", "path", "random", "regex", "semver", "str", "time", "toml", "url", "yaml"
            ]
        );
    }

    /// An object return type (R-6): a function's return shape written
    /// out in full, found past its balanced braces.
    #[test]
    fn an_object_return_type_is_one_type() {
        let f = registry().get("oci.parse").unwrap();
        assert_eq!(
            f.ret,
            "{ registry: string?, repository: string, tag: string?, digest: string? }"
        );
        assert!(f.partial);
    }

    #[test]
    fn a_signature_file_is_checked() {
        let bad = |text: &str| Registry::load(&[("t.df", text)]).unwrap_err();
        assert!(bad("fn f(a: int) -> int").contains("before `package`"));
        assert!(bad("package p\nfn f(a) -> int").contains("has no type"));
        assert!(bad("package p\nfn f(a: int) -> int sometimes").contains("unknown flag"));
        assert!(bad("package p\nfn f(a: int) -> int\nfn f(b: int) -> int").contains("twice"));
        assert!(bad("package prelude\nfn inet(s: string) -> string").contains("constructor"));
        assert!(Registry::load(&[("t.df", "package prelude\nfn inet(s: string) -> inet")]).is_ok());
        let two = Registry::load(&[
            ("a.df", "package prelude\nfn geo(s: string) -> string"),
            ("b.df", "package geo\nfn distance(a: int, b: int) -> int"),
        ]);
        assert!(two.unwrap_err().contains("only a constructor"));
    }

    /// Every function `std/*.df` declares has a body, here or in
    /// `engine::BODIES` (R-6: `engine::body` falls back to this
    /// module's own table).
    #[test]
    fn every_declared_function_has_a_body() {
        for f in registry().functions() {
            assert!(
                crate::engine::body(&f.name).is_some(),
                "{} ({}:{}) has no body",
                f.name,
                f.file,
                f.line
            );
        }
    }

    /// The reverse: a body here is declared somewhere (no orphan).
    #[test]
    fn every_body_here_is_declared() {
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
/// ([`random::master`]): a deployment's evaluation sets it on its thread; with
/// none (an editor, a bare evaluation) the functions have no value.
pub mod random {
    use crate::value::Value;
    use std::cell::RefCell;

    thread_local! {
        static MASTER: RefCell<Option<(Vec<u8>, String)>> = const { RefCell::new(None) };
    }

    /// Derive this thread's `random.*` from `ikm` for `deployment` until
    /// the next call.
    pub fn set_master(ikm: Vec<u8>, deployment: &str) {
        MASTER.with(|m| *m.borrow_mut() = Some((ikm, deployment.to_string())));
    }

    /// The master's input key material: `RANDOM_MASTER` from the
    /// environment, else what the stack's key derives for it (`key`).
    pub fn master(
        key: impl FnOnce() -> anyhow::Result<crate::zset::file::Key>,
    ) -> anyhow::Result<Vec<u8>> {
        match std::env::var("RANDOM_MASTER") {
            Ok(m) if !m.is_empty() => Ok(m.into_bytes()),
            _ => Ok(crate::secrets::derived(&key()?, "random master").to_vec()),
        }
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
            let (ikm, deployment) = m.as_ref()?;
            let mut info = Vec::new();
            for part in [what, deployment.as_str(), key].iter().chain(knobs) {
                info.extend_from_slice(part.as_bytes());
                info.push(0);
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

    pub fn password(a: &[Value]) -> Option<Value> {
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
        chars("password", key, &[&n, name], length as usize, &set).map(Value::Str)
    }

    pub fn bytes(a: &[Value]) -> Option<Value> {
        use base64::Engine;
        let [Value::Str(key), Value::Int(n)] = a else {
            return None;
        };
        if !(1..=4096).contains(n) {
            return None;
        }
        let b = derive("bytes", key, &[&n.to_string()], *n as usize)?;
        Some(Value::Str(
            base64::engine::general_purpose::STANDARD.encode(b),
        ))
    }

    pub fn id(a: &[Value]) -> Option<Value> {
        let (key, n) = match a {
            [Value::Str(k)] => (k, 8),
            [Value::Str(k), Value::Int(n)] => (k, *n),
            _ => return None,
        };
        if !(1..=64).contains(&n) {
            return None;
        }
        let b = derive("id", key, &[&n.to_string()], n as usize)?;
        Some(Value::Str(b.iter().map(|x| format!("{x:02x}")).collect()))
    }

    pub fn uuid(a: &[Value]) -> Option<Value> {
        let [Value::Str(key)] = a else { return None };
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
    }

    /// Synapse's signing key file: `ed25519 a_XXXX SEED`, the version four
    /// letters and the 32-byte seed unpadded base64 (what
    /// `generate_signing_key` writes).
    pub fn signing_key(a: &[Value]) -> Option<Value> {
        use base64::Engine;
        let [Value::Str(key)] = a else { return None };
        let version = chars("signing_key version", key, &[], 4, &ALNUM[..52])?;
        let seed = derive("signing_key ed25519", key, &[], 32)?;
        Some(Value::Str(format!(
            "ed25519 a_{version} {}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(seed)
        )))
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
        set_master(b"m1".to_vec(), "app");
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
        let b = s(bytes(&[k("cookie"), Value::Int(32)]));
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
        set_master(b"m1".to_vec(), "app[env=prod]");
        assert_ne!(pw, s(password(&[k("db")])), "the deployment is in it");
        set_master(b"m2".to_vec(), "app");
        assert_ne!(pw, s(password(&[k("db")])), "the master is in it");
    }

    #[test]
    fn with_no_master_there_is_no_value() {
        assert!(password(&[Value::Str("db".into())]).is_none());
    }
}
