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
        if !crate::lexer::is_word(n) || t.is_empty() {
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

/// A function's body: its arguments' values to its value, or none.
pub type Body = fn(&[Value]) -> Option<Value>;

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
                crate::quantity::scale(q, *n).map(Value::Quantity)
            }
            _ => None,
        })
    }),
    ("div", |a| {
        num2(a, |x, y| (y != 0).then(|| x / y), |x, y| x / y).or_else(|| match a {
            [Value::Quantity(q), Value::Int(n)] => {
                crate::quantity::divide(q, *n).map(Value::Quantity)
            }
            [Value::Quantity(p), Value::Quantity(q)] => {
                crate::quantity::ratio(p, q).map(Value::Int)
            }
            _ => None,
        })
    }),
    ("mod", |a| {
        num2(a, |x, y| (y != 0).then(|| x % y), |x, y| x % y)
    }),
    // Constructors (DESIGN.org "Silent string-to-int coercion"):
    // conversions are explicit and named by their type.
    ("int", |a| match a {
        [Value::Int(i)] => Some(Value::Int(*i)),
        // Toward zero; none past an int's range.
        [Value::Float(f)] => {
            let t = f.get().trunc();
            (t >= i64::MIN as f64 && t < i64::MAX as f64).then_some(Value::Int(t as i64))
        }
        [Value::Str(s)] => s.trim().parse().ok().map(Value::Int),
        _ => None,
    }),
    ("float", |a| match a {
        [f @ Value::Float(_)] => Some(f.clone()),
        [Value::Int(i)] => crate::value::Float::new(*i as f64).map(Value::Float),
        [Value::Str(s)] => crate::value::Float::parse(s).ok().map(Value::Float),
        _ => None,
    }),
    ("string", |a| match a {
        [v] => scalar_text(v).map(Value::Str),
        _ => None,
    }),
    ("ip", |a| match a {
        [Value::Str(s)] => crate::value::ipv4_to_u32(s).map(Value::Ip),
        _ => None,
    }),
    ("inet", |a| match a {
        [Value::Str(s)] => {
            let (addr, prefix) = crate::value::parse_ipnet(s)?;
            Some(Value::IpNet { addr, prefix })
        }
        _ => None,
    }),
    // Quantities and times (R-66, R-62): a value of the type is itself,
    // its text is read, an integer is bytes or cores.
    ("bytes", |a| quantity_of(a, crate::quantity::Dim::Bytes)),
    ("cpu", |a| quantity_of(a, crate::quantity::Dim::Cpu)),
    ("duration", |a| {
        quantity_of(a, crate::quantity::Dim::Duration)
    }),
    ("time", |a| match a {
        [t @ Value::Time(_)] => Some(t.clone()),
        [Value::Str(s)] => crate::time::Time::parse(s).ok().map(Value::Time),
        _ => None,
    }),
    // An ambiguous quantity no position read has no value; the compiler
    // says so where the schema is known (`types::read`).
    (crate::types::AMBIGUOUS, |_| None),
    ("time.parse", |a| match a {
        [Value::Str(s)] => crate::time::Time::parse(s).ok().map(Value::Time),
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
    ("time.add", |a| match a {
        [
            Value::Time(t),
            Value::Quantity(crate::quantity::Quantity::Duration(d)),
        ] => t.add(*d).map(Value::Time),
        _ => None,
    }),
    ("time.until", |a| match a {
        [Value::Time(x), Value::Time(y)] => x
            .until(y)
            .map(|d| Value::Quantity(crate::quantity::Quantity::Duration(d))),
        _ => None,
    }),
    ("time.before", |a| match a {
        [Value::Time(x), Value::Time(y)] => Some(Value::Bool(x.instant() < y.instant())),
        _ => None,
    }),
    ("duration.parse", |a| match a {
        [Value::Str(s)] => crate::quantity::read_duration(s)
            .ok()
            .map(|d| Value::Quantity(crate::quantity::Quantity::Duration(d))),
        _ => None,
    }),
    ("duration.total", unit_of),
    ("bytes.to", unit_of),
    ("cpu.to", unit_of),
    ("iprange", |a| match a {
        [x, y] => {
            let (sa, sb) = (as_ip_u32(x)?, as_ip_u32(y)?);
            let (start, end) = if sa <= sb { (sa, sb) } else { (sb, sa) };
            Some(Value::IpRange { start, end })
        }
        _ => None,
    }),
    ("format", |a| {
        let fmt = a.first()?.as_str()?;
        let mut out = String::new();
        let mut parts = fmt.split("%s");
        out.push_str(parts.next().unwrap_or(""));
        for (i, p) in parts.enumerate() {
            out.push_str(&value_to_string(a.get(i + 1)?));
            out.push_str(p);
        }
        Some(Value::Str(out))
    }),
    ("len", len_of),
    ("list.len", len_of),
    ("ref", |a| match a {
        [Value::Str(t), Value::Str(n), Value::Str(p)] => Some(Value::Ref {
            typ: t.clone(),
            name: n.clone(),
            attr: p.clone(),
        }),
        // `ref(r)` written out (R-43): the reference itself.
        [r @ Value::Ref { .. }] => Some(r.clone()),
        _ => None,
    }),
    ("cloud_ref", |a| match a {
        [Value::Str(t), Value::Str(n), Value::Str(p)] => Some(Value::CloudRef {
            typ: t.clone(),
            name: n.clone(),
            attr: p.clone(),
        }),
        _ => None,
    }),
    // A name that is already an address (another copy's resource, read
    // through its output) is itself (R-65).
    ("scoped", |a| match a {
        [_, Value::Str(name)] if crate::ir::is_scoped(name) => Some(Value::Str(name.clone())),
        [scope, name] => Some(Value::Str(crate::ir::scoped(
            &value_to_string(scope),
            &value_to_string(name),
        ))),
        _ => None,
    }),
    // E DR-19: `declassify(V, Reason)` is `V`; the static pass reads it
    // as public, and `declassified/2` records it (`transform`).
    ("declassify", |a| match a {
        [v, _] => Some(v.clone()),
        _ => None,
    }),
    // The prelude's null for (T, A, P) (E §2.5): class and type come
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
    ("__path", |a| match a {
        [v, path] => {
            let mut v = v.clone();
            for seg in path.as_str()?.split('.') {
                // A url's components read as an object's (`u.host`).
                if let Value::Url(u) = &v {
                    v = crate::value::url_parts(u)?;
                }
                let Value::Obj(mut m) = v else {
                    return None;
                };
                v = m.remove(seg)?;
            }
            Some(v)
        }
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
    ("inet.addr", |a| match a {
        [net, n] => {
            let (addr, _) = as_ipnet(net)?;
            let idx = as_i64(n)?;
            if idx < 0 {
                return None;
            }
            Some(Value::Ip(addr.wrapping_add(idx as u32)))
        }
        _ => None,
    }),
    ("inet.contains", |a| match a {
        [net, ip] => {
            let (addr, prefix) = as_ipnet(net)?;
            let n = as_ip_u32(ip)?;
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix as u32)
            };
            Some(Value::Bool((n & mask) == addr))
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
    ("inet.prefix_len", |a| match a {
        [net] => as_ipnet(net).map(|(_, p)| Value::Int(p as i64)),
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
    ("random.bytes", crate::functions::random::bytes),
    ("random.id", crate::functions::random::id),
    ("random.uuid", crate::functions::random::uuid),
    ("random.signing_key", crate::functions::random::signing_key),
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
            regex::Regex::new(re)
                .ok()?
                .replace_all(s, with.as_str())
                .into_owned(),
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
        [Value::Str(s)] => Some(Value::Str(crate::approval::sha256_hex(s.as_bytes()))),
        _ => None,
    }),
    ("hash.short", |a| match a {
        [Value::Str(s), Value::Int(n)] => {
            let full = crate::approval::sha256_hex(s.as_bytes());
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
        [u @ Value::Url(_)] => Some(u.clone()),
        [Value::Str(s)] => crate::value::parse_url(s).ok(),
        _ => None,
    }),
    ("url.parse", |a| match a {
        [v] => crate::value::url_parts(&as_url(v)?),
        _ => None,
    }),
    ("url.join", |a| match a {
        [Value::Str(x), Value::Str(y)] => Some(Value::Str(join_slash(x, y))),
        _ => None,
    }),
    ("url.with_scheme", |a| match a {
        [u, Value::Str(scheme)] => with_url(u, |u| u.set_scheme(scheme).ok()),
        _ => None,
    }),
    ("url.with_host", |a| match a {
        [u, Value::Str(host)] => with_url(u, |u| u.set_host(Some(host)).ok()),
        _ => None,
    }),
    ("url.with_port", |a| match a {
        [u, Value::Int(port)] => {
            let p = u16::try_from(*port).ok()?;
            with_url(u, |u| u.set_port(Some(p)).ok())
        }
        _ => None,
    }),
    ("url.with_path", |a| match a {
        [u, Value::Str(path)] => with_url(u, |u| {
            u.set_path(path);
            Some(())
        }),
        _ => None,
    }),
    ("url.with_query", |a| match a {
        [u, Value::Obj(q)] => {
            let pairs: Option<Vec<(&String, &String)>> = q
                .iter()
                .map(|(k, v)| match v {
                    Value::Str(s) => Some((k, s)),
                    _ => None,
                })
                .collect();
            let pairs = pairs?;
            with_url(u, |u| {
                if pairs.is_empty() {
                    u.set_query(None);
                } else {
                    let mut qs = url::form_urlencoded::Serializer::new(String::new());
                    for (k, v) in &pairs {
                        qs.append_pair(k, v);
                    }
                    u.set_query(Some(&qs.finish()));
                }
                Some(())
            })
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
        [Value::Str(s)] => crate::tables::document("json", s).ok(),
        _ => None,
    }),
    ("json.encode", |a| match a {
        [v] if encodable(v) => serde_json::to_string(&crate::engine::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
    ("yaml.decode", |a| match a {
        [Value::Str(s)] => crate::tables::document("yaml", s).ok(),
        _ => None,
    }),
    ("yaml.encode", |a| match a {
        [v] if encodable(v) => serde_yaml::to_string(&crate::engine::value_to_json(v))
            .ok()
            .map(Value::Str),
        _ => None,
    }),
    ("toml.decode", |a| match a {
        [Value::Str(s)] => crate::tables::document("toml", s).ok(),
        _ => None,
    }),
    ("toml.encode", |a| match a {
        [v] if encodable(v) => toml::to_string(&crate::engine::value_to_json(v))
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
        Value::IpRange { start, end } => format!(
            "{}-{}",
            crate::value::u32_to_ipv4(*start),
            crate::value::u32_to_ipv4(*end)
        ),
        // H-16: a reference reads as the source names it, `T["A"].path`.
        Value::Ref { typ, name, attr } => crate::ir::Address {
            typ: typ.clone(),
            name: name.clone(),
        }
        .attr(attr),
        Value::CloudRef { typ, name, attr } => format!("cloud_ref({typ},{name},{attr})"),
        Value::Null { label, .. } => format!("?{label}"),
        Value::Quantity(q) => q.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Url(u) => u.clone(),
    }
}

fn len_of(a: &[Value]) -> Option<Value> {
    match a {
        [Value::List(xs)] => Some(Value::Int(xs.len() as i64)),
        [Value::Obj(m)] => Some(Value::Int(m.len() as i64)),
        [Value::Str(s)] => Some(Value::Int(s.chars().count() as i64)),
        _ => None,
    }
}

/// A constructor's quantity: one of its dimension is itself, a string is
/// read, an integer is bytes or cores.
fn quantity_of(a: &[Value], dim: crate::quantity::Dim) -> Option<Value> {
    use crate::quantity::{Dim, Quantity, read};
    match (a, dim) {
        ([Value::Quantity(q)], d) if q.dim() == d => Some(Value::Quantity(*q)),
        ([Value::Str(s)], d) => read(d, s).ok().map(Value::Quantity),
        // A decimal is cores (`cpu(0.5)` is `500m`).
        ([Value::Float(f)], Dim::Cpu) => read(Dim::Cpu, &f.to_string()).ok().map(Value::Quantity),
        ([Value::Int(n)], Dim::Bytes) => Some(Value::Quantity(Quantity::Bytes(*n))),
        ([Value::Int(n)], Dim::Cpu) => n
            .checked_mul(1000)
            .map(|m| Value::Quantity(Quantity::Cpu(m))),
        _ => None,
    }
}

/// A quantity as a whole number of a unit (`bytes.to`, `cpu.to`,
/// `duration.total`), of its own dimension only.
fn unit_of(a: &[Value]) -> Option<Value> {
    match a {
        [Value::Quantity(q), Value::Str(unit)] => crate::quantity::to_unit(q, unit).map(Value::Int),
        _ => None,
    }
}

/// `a + b` (`a - b`) of quantities of one dimension, or of a time and a
/// duration (R-66, R-62); none across dimensions.
fn measured(a: &[Value], sub: bool) -> Option<Value> {
    use crate::quantity::{Quantity, add};
    match a {
        [Value::Quantity(x), Value::Quantity(y)] => add(x, y, sub).map(Value::Quantity),
        [Value::Time(t), Value::Quantity(Quantity::Duration(d))] => {
            t.add(if sub { d.negate() } else { *d }).map(Value::Time)
        }
        [Value::Quantity(Quantity::Duration(d)), Value::Time(t)] if !sub => {
            t.add(*d).map(Value::Time)
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

/// A url argument's canonical text: a url, or a string read as one (a
/// string in a `url` position parses at the edge).
fn as_url(v: &Value) -> Option<String> {
    match v {
        Value::Url(u) => Some(u.clone()),
        Value::Str(s) => url::Url::parse(s).ok().map(|u| u.to_string()),
        _ => None,
    }
}

/// The url `u` with `set` applied (`url.with_host`, ..): a url, or no
/// value when `u` is not one or `set` refuses.
fn with_url(u: &Value, set: impl FnOnce(&mut url::Url) -> Option<()>) -> Option<Value> {
    let mut u = url::Url::parse(&as_url(u)?).ok()?;
    set(&mut u)?;
    Some(Value::Url(u.to_string()))
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
        Value::Quantity(_) | Value::Time(_) | Value::Url(_) => v.typed_text(),
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
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn is_registry(s: &str) -> bool {
    !s.is_empty() && (s.contains('.') || s.contains(':') || s == "localhost")
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

    /// Every function `std/*.df` declares has a body (DESIGN.org R-6:
    /// one registry).
    #[test]
    fn every_declared_function_has_a_body() {
        for f in registry().functions() {
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
        assert_eq!(registry().get("oci.parse").unwrap().params.len(), 1);
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
/// ([`random::master`]): a deployment's evaluation sets it on its thread; with
/// none (an editor, a bare evaluation) the functions have no value.
pub mod random {
    use crate::value::Value;
    use std::cell::RefCell;

    thread_local! {
        static MASTER: RefCell<Option<(Vec<u8>, String)>> = const { RefCell::new(None) };
        /// The secrets this thread derived since its master was set, each
        /// by the call that derived it (`random.password("db")`): what the
        /// plan, `query` and `why` label one with.
        static DERIVED: RefCell<std::collections::BTreeMap<Value, String>> =
            const { RefCell::new(std::collections::BTreeMap::new()) };
    }

    /// Derive this thread's `random.*` from `ikm` for `deployment` until
    /// the next call.
    pub fn set_master(ikm: Vec<u8>, deployment: &str) {
        MASTER.with(|m| *m.borrow_mut() = Some((ikm, deployment.to_string())));
        DERIVED.with(|d| d.borrow_mut().clear());
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

    /// `v`, a secret derived by `random.WHAT(key, ..)`, labelled.
    fn secret(what: &str, key: &str, v: Option<Value>) -> Option<Value> {
        let v = v?;
        DERIVED.with(|d| {
            d.borrow_mut()
                .entry(v.clone())
                .or_insert_with(|| format!("random.{what}({key:?})"));
        });
        Some(v)
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
        secret(
            "password",
            key,
            chars("password", key, &[&n, name], length as usize, &set).map(Value::Str),
        )
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
        secret(
            "bytes",
            key,
            Some(Value::Str(
                base64::engine::general_purpose::STANDARD.encode(b),
            )),
        )
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
        secret(
            "signing_key",
            key,
            Some(Value::Str(format!(
                "ed25519 a_{version} {}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(seed)
            ))),
        )
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
