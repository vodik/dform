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

/// The signature files, by the path they are shipped at.
pub const SOURCES: &[(&str, &str)] = &[
    ("std/prelude.df", include_str!("../../../std/prelude.df")),
    ("std/inet.df", include_str!("../../../std/inet.df")),
    ("std/int.df", include_str!("../../../std/int.df")),
    ("std/ip.df", include_str!("../../../std/ip.df")),
    ("std/str.df", include_str!("../../../std/str.df")),
    ("std/list.df", include_str!("../../../std/list.df")),
];

/// The package whose functions are written bare.
pub const PRELUDE: &str = "prelude";

/// Type names a prelude function may share only as that type's
/// constructor (`inet(s) -> inet`).
const TYPE_NAMES: &[&str] = &[
    "int", "string", "bool", "inet", "ip", "iprange", "list", "any", "ref", "secret", "symbol",
    "addr",
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

/// `name(p: T, ...) -> T[?] [flag, ...]`, the part after `fn `.
fn function(sig: &str) -> Result<Function, String> {
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
    let mut depth = 0;
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
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

/// Where a type at the start of `s` ends: a name, then `( .. )` if any.
fn type_end(s: &str) -> usize {
    let name = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(s.len());
    if s[name..].starts_with('(') {
        matching(s, name).map_or(s.len(), |c| c + 1)
    } else {
        name
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
        assert_eq!(r.packages(), ["inet", "int", "ip", "list", "str"]);
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
}
