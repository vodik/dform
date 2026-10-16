//! The column types' lattice (R-34): what a literal's kind is, which
//! types are ever equal and which the narrower or the wider of two, and
//! what each says is wrong of a column, as [`super::columns`] settles
//! and [`super::checks`] judges them.

use crate::ast::{Span, Term};
use crate::diag::Diagnostic;
use crate::spell;
use crate::types::{self, Ty};
use crate::value::Value;

/// Relations the compiler writes and reads with values of any type: not a
/// program's relation, never typed by its uses.
pub(crate) fn untyped(pred: &str) -> bool {
    crate::loader::is_core_pred(pred)
        || pred.contains("__")
        || pred.starts_with("type_")
        || matches!(
            pred,
            crate::transform::SECRET_CELL
                | "instance_of"
                | "settings_row"
                | "stack_output"
                | "provider_config"
                | "provider_expect_account"
                | "let"
        )
}

/// A function's call, not a relation's atom.
pub(crate) fn function(pred: &str) -> Option<&'static crate::functions::Function> {
    crate::functions::get(pred)
}

/// What fixed a type, for the message.
#[derive(Debug, Clone)]
pub(crate) struct Hard {
    pub(crate) ty: Ty,
    pub(crate) span: Span,
    pub(crate) what: String,
    /// The column it was given to, when it was given to one (not to a
    /// variable).
    pub(crate) col: Option<super::Col>,
    /// A builtin's `string` parameter: a value type is given to it as its
    /// print (R-133), so beside one it constrains nothing.
    pub(crate) prints: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Literal {
    pub(crate) value: Value,
    pub(crate) span: Span,
}

/// What flowed into a cell: its type, the variable as written, the rule.
pub(crate) struct Given {
    pub(crate) ty: Ty,
    pub(crate) var: String,
    pub(crate) what: String,
    pub(crate) span: Span,
}

/// The column `col` of the type `f` says: what flows in (`given`) is
/// checked assignable to it and never narrows it (R-213), and its
/// literals `lits` are checked against it.
pub(crate) fn declared(
    f: Hard,
    col: &str,
    given: &[Given],
    lits: &[Literal],
    diags: &mut Vec<Diagnostic>,
) -> Option<Ty> {
    if let Some(g) = given.iter().find(|g| !compatible(&g.ty, &f.ty)) {
        let msg = if f.what.starts_with("decl ") || f.what == col {
            format!("{col}: {} takes {}: {}", f.ty, g.var, not_a(&g.ty, &f.ty))
        } else {
            format!(
                "{col} is {} ({}) and takes {}: {}",
                f.ty,
                f.what,
                g.var,
                not_a(&g.ty, &f.ty)
            )
        };
        diags.push(Diagnostic::error(f.span, msg).with_label(
            g.span,
            format!("{} is {} here ({})", g.var, a(&g.ty), g.what),
        ));
        return None;
    }
    let map = matches!(&f.ty, Ty::Map(_));
    for l in lits
        .iter()
        .filter(|l| map || !matches!(l.value, Value::Obj(_)))
    {
        if let Some(why) = types::mismatch(&f.ty, &Term::Val(l.value.clone())) {
            diags.push(
                Diagnostic::error(l.span, format!("{col} {why}"))
                    .with_label(f.span, format!("{col} is {} here ({})", f.ty, f.what)),
            );
        }
    }
    Some(f.ty)
}

/// The column `col` of literals only (`lits`): their kind; a string
/// where nothing says otherwise. Two kinds are the error.
pub(crate) fn of_literals(col: &str, lits: &[Literal], diags: &mut Vec<Diagnostic>) -> Option<Ty> {
    let mut first: Option<(&Literal, Ty)> = None;
    let mut string: Option<&Literal> = None;
    for l in lits {
        match kind(&l.value) {
            None if matches!(l.value, Value::Obj(_)) => {}
            None => {
                string.get_or_insert(l);
            }
            Some(k) => match &mut first {
                None => first = Some((l, k)),
                Some((f, fk)) if !compatible(fk, &k) => {
                    diags.push(
                        Diagnostic::error(
                            l.span,
                            format!("{col} holds {} here and {fk} elsewhere", shown(&l.value)),
                        )
                        .with_label(f.span, format!("{} here", shown(&f.value))),
                    );
                    return None;
                }
                Some((_, fk)) => *fk = narrower(fk.clone(), &k),
            },
        }
    }
    match (first, string) {
        (Some((l, k)), Some(s)) => {
            diags.push(
                Diagnostic::error(
                    l.span,
                    format!("{col} holds {} here and strings elsewhere", shown(&l.value)),
                )
                .with_label(s.span, format!("{} here", shown(&s.value)))
                .with_help(format!(
                    "one column has one type: write the {k} as one everywhere, or declare \
                     the column"
                )),
            );
            None
        }
        (Some((_, k)), None) => Some(k),
        (None, Some(_)) => Some(Ty::Scalar("string".into())),
        (None, None) => None,
    }
}

/// The side `t` of the arithmetic `written`, of type `ty`, when it is not
/// a number, a quantity or a time.
pub(crate) fn not_a_number(written: &str, t: &Term, ty: &Ty, span: Span) -> Option<Diagnostic> {
    let number = matches!(ty, Ty::Scalar(s) if number(s) || matches!(s.as_str(),
        "bytes" | "cpu" | "duration" | "time"));
    (!number && !matches!(ty, Ty::Any | Ty::Secret(_))).then(|| {
        Diagnostic::error(
            span,
            format!(
                "{written}: `{}` is {ty}, and arithmetic is on numbers, \
                 quantities and times",
                shown_term(t)
            ),
        )
    })
}

/// The type a call's result has, by its signature; a reference's, its
/// type's (`ref(T, A, "")`).
pub(crate) fn term_type(t: &Term) -> Option<(Ty, String)> {
    let Term::Func { name, args } = t else {
        return None;
    };
    if name == crate::address::REF && args.len() == 3 {
        return match &args[0] {
            Term::Val(Value::Str(typ)) => Some((Ty::Ref(typ.clone()), "a reference".into())),
            _ => None,
        };
    }
    if arithmetic(name).is_some() {
        return None;
    }
    let f = function(name)?;
    let ty = Ty::parse(f.ret.trim_end_matches('?'));
    (ty != Ty::Any).then(|| (ty, format!("`{name}`'s result")))
}

/// An object literal of constants as its value (`{ x: "big" }`).
pub(crate) fn constant(t: &Term) -> Option<Value> {
    match t {
        Term::Val(v) => Some(v.clone()),
        Term::Obj(m) => m
            .iter()
            .map(|(k, x)| Some((k.clone(), constant(x)?)))
            .collect::<Option<_>>()
            .map(Value::Obj),
        _ => None,
    }
}

/// `add` and its kin: the operator they are written with.
pub(crate) fn arithmetic(name: &str) -> Option<&'static str> {
    Some(match name {
        "add" => "+",
        "sub" => "-",
        "mul" => "*",
        "div" => "/",
        "mod" => "%",
        _ => return None,
    })
}

/// A literal's type, `None` for a string (unknown until its column is
/// settled) and for what the pass does not judge.
pub(crate) fn kind(v: &Value) -> Option<Ty> {
    let s = |x: &str| Some(Ty::Scalar(x.into()));
    match v {
        Value::Int(_) => s("int"),
        Value::Float(_) => s("float"),
        Value::Bool(_) => s("bool"),
        Value::IpNet { .. } => s("inet"),
        Value::Ip(_) => s("ip"),
        Value::Quantity(q) => s(q.dim().name()),
        Value::Time(_) => s("time"),
        Value::Uri(_) => s("uri"),
        Value::Oci(_) => s("oci"),
        Value::Semver(_) => s("semver"),
        Value::Range(r) => s(&format!("range({})", r.element()?)),
        _ => None,
    }
}

/// Whether a value of `a` can ever equal one of `b`.
pub(crate) fn compatible(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        (Ty::Any, _) | (_, Ty::Any) => true,
        (Ty::Secret(x), y) | (y, Ty::Secret(x)) => compatible(x, y),
        (Ty::Enum(_), Ty::Enum(_)) => true,
        (Ty::Enum(_), Ty::Scalar(s)) | (Ty::Scalar(s), Ty::Enum(_)) => s == "string",
        // A network or an address is written as text, and a function
        // reads text as one (`inet.subnet(vpc.cidr, ..)`, the cidr a
        // string attribute): a string column holding them is the narrower
        // type.
        (Ty::Scalar(x), Ty::Scalar(y)) if text_of(x, y) || text_of(y, x) => true,
        // Numbers compare by value (R-75).
        (Ty::Scalar(x), Ty::Scalar(y)) if number(x) && number(y) => true,
        (Ty::List(x), Ty::List(y)) | (Ty::Map(x), Ty::Map(y)) => compatible(x, y),
        (Ty::Object(x), Ty::Object(y)) => x == y,
        (Ty::Ref(x), Ty::Ref(y)) => Ty::ref_types(x).any(|t| Ty::ref_types(y).any(|u| t == u)),
        (Ty::Scalar(x), Ty::Scalar(y)) => x == y,
        _ => false,
    }
}

/// A value type with a canonical print, given as it where a string is
/// wanted: an `oci`, a `uri`, a network or an address, a time, a quantity.
pub(crate) fn printed(ty: &Ty) -> bool {
    matches!(ty, Ty::Scalar(s) if matches!(s.as_str(),
        "oci" | "uri" | "inet" | "ip" | "time" | "bytes" | "cpu" | "duration"
        | "semver") || crate::range::element(s).is_some())
}

/// A type whose values have no fields to read: a string, a number, a
/// bool, an address, a quantity or a time, a list, a reference. A uri, an
/// image reference, a network, a version and a range have their parts.
pub(crate) fn fieldless(ty: &Ty) -> bool {
    match ty {
        Ty::Secret(t) => fieldless(t),
        Ty::Scalar(s) => {
            !matches!(s.as_str(), "uri" | "oci" | "inet" | "semver")
                && crate::range::element(s).is_none()
        }
        Ty::Enum(_) | Ty::List(_) | Ty::Ref(_) => true,
        Ty::Map(_) | Ty::Object(_) | Ty::Any => false,
    }
}

/// `int`, `float`, `number` (either).
fn number(ty: &str) -> bool {
    matches!(ty, "int" | "float" | "number")
}

/// `narrow` is read from a string: an `inet`, an `ip`, an image
/// reference (R-133: and is its text where a string is wanted).
fn text_of(string: &str, narrow: &str) -> bool {
    string == "string"
        && (matches!(narrow, "inet" | "ip" | "oci" | "semver")
            || crate::range::element(narrow).is_some())
}

/// The more telling of two compatible types: an enum over a string, a
/// typed list over an untyped one; an int and a float are numbers.
pub(crate) fn narrower(a: Ty, b: &Ty) -> Ty {
    match (&a, b) {
        (Ty::Scalar(x), Ty::Scalar(y)) if x != y && number(x) && number(y) => {
            Ty::Scalar("number".into())
        }
        (Ty::Any, _) => b.clone(),
        (Ty::Scalar(s), Ty::Enum(_)) if s == "string" => b.clone(),
        (Ty::Scalar(s), Ty::Scalar(n)) if text_of(s, n) => b.clone(),
        (Ty::List(x), Ty::List(_)) if **x == Ty::Any => b.clone(),
        _ => a,
    }
}

/// A term as the program wrote it, near enough for a message.
pub(crate) fn shown_term(t: &Term) -> String {
    match t {
        Term::Var(v) => shown_var(v),
        Term::Func { name, args } if name == "__path" => match args.as_slice() {
            [Term::Var(v), Term::Val(Value::Str(path))] => format!("{}.{path}", shown_var(v)),
            _ => spell::term(t),
        },
        t => spell::term(t),
    }
}

/// A lowered variable as the program wrote it: `Zone` is `zone`; a fresh
/// one's suffix dropped.
pub(crate) fn shown_var(v: &str) -> String {
    let base = v.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    let base = if base.is_empty() { v } else { base };
    let mut out = String::new();
    for (i, ch) in base.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// An untyped cell's type: the wider of what flows into it (`g` and
/// `rest`) and its literals, an enum and a string a string (R-213).
pub(crate) fn widened(
    col: &str,
    g: &Given,
    rest: &[Given],
    lits: &[Literal],
    diags: &mut Vec<Diagnostic>,
) -> Option<Ty> {
    let mut ty = g.ty.clone();
    for h in rest {
        match wider(&ty, &h.ty) {
            Some(t) => ty = t,
            None => {
                diags.push(
                    Diagnostic::error(
                        h.span,
                        format!(
                            "{col} takes {} here, {}, and {} elsewhere, {}: one value has \
                             one type",
                            h.var,
                            a(&h.ty),
                            g.var,
                            a(&g.ty)
                        ),
                    )
                    .with_label(
                        g.span,
                        format!("{} is {} here ({})", g.var, a(&g.ty), g.what),
                    ),
                );
                return None;
            }
        }
    }
    let map = matches!(&ty, Ty::Map(_));
    for l in lits
        .iter()
        .filter(|l| map || !matches!(l.value, Value::Obj(_)))
    {
        let wide = match (&l.value, kind(&l.value)) {
            (Value::Str(_), _) if types::mismatch(&ty, &Term::Val(l.value.clone())).is_none() => {
                continue;
            }
            (Value::Str(_), _) => to_string(&ty).then(|| Ty::Scalar("string".into())),
            (_, Some(k)) => wider(&ty, &k),
            _ => continue,
        };
        match wide {
            Some(t) => ty = t,
            None => {
                diags.push(
                    Diagnostic::error(
                        l.span,
                        format!(
                            "{col} takes {} here, and {} elsewhere, {}: one value has one type",
                            shown(&l.value),
                            g.var,
                            a(&g.ty)
                        ),
                    )
                    .with_label(
                        g.span,
                        format!("{} is {} here ({})", g.var, a(&g.ty), g.what),
                    ),
                );
                return None;
            }
        }
    }
    Some(ty)
}

/// The comparison `written` of `a` and `b` (each its term and its type,
/// as the columns settled): a reference compared with a string, a string
/// that is no value of its other side's type, two types never equal.
pub(crate) fn compared(
    written: &str,
    (a, ta): (&Term, Option<&Ty>),
    (b, tb): (&Term, Option<&Ty>),
    span: Span,
) -> Option<Diagnostic> {
    match (ta, tb, a, b) {
        (Some(Ty::Ref(want)), _, x, Term::Val(Value::Str(text)))
        | (_, Some(Ty::Ref(want)), Term::Val(Value::Str(text)), x) => {
            Some(ref_and_string(written, &shown_term(x), want, text, span))
        }
        (Some(t), _, x, Term::Val(v)) | (_, Some(t), Term::Val(v), x)
            if matches!(v, Value::Str(_)) =>
        {
            types::mismatch(t, &Term::Val(v.clone()))
                .map(|why| Diagnostic::error(span, format!("{written}: `{}` {why}", shown_term(x))))
        }
        (Some(a), Some(b), _, _) if !compatible(a, b) => Some(Diagnostic::error(
            span,
            format!("{written} compares {a} with {b}: they are never equal"),
        )),
        _ => None,
    }
}

/// `x in o` over the object `o`: its entries are matched by a pattern,
/// `(k, v) in o` (R-58).
pub(crate) fn over_object(x: &str, o: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        span,
        format!(
            "`{x} in {o}`: `{o}` is an object, and an object's entries are matched by a pattern"
        ),
    )
    .with_help(format!(
        "`(k, v) in {o}` takes each key and value, `(k, _) in {o}` each key (R-58)"
    ))
}

/// `x.path` where `x` is `ty`, of the column `col`, which has no fields:
/// the error, where it read nothing at run time (R-185).
pub(crate) fn no_field(x: &str, path: &str, ty: &Ty, col: &str, span: Span) -> Option<Diagnostic> {
    let first = crate::address::path_keys(path).into_iter().next()?;
    let read = format!("{x}.{path}");
    let help = match ty {
        Ty::Ref(t) => format!(
            "bind it by its type where the column is filled, `{x} in {}`: the column then \
             holds the resource, and `{read}` reads its attribute",
            Ty::ref_types(t).next().unwrap_or(t)
        ),
        _ => format!(
            "a resource's attributes are read through a reference: bind one by its type \
             where the column is filled (`{x} in k8s.deployment`), and `{read}` reads it"
        ),
    };
    Some(
        Diagnostic::error(
            span,
            format!("`{read}`: `{x}` is {ty} ({col}), which has no field `{first}`"),
        )
        .with_help(help),
    )
}

/// The error of a column that `h` (in the column `hc`) says is one type
/// and `f` (in `fc`) another.
pub(crate) fn disjoint(h: &Hard, f: &Hard, hc: String, fc: String) -> Diagnostic {
    let msg = if hc == fc {
        format!("{hc} is {} here ({}) and {} elsewhere", h.ty, h.what, f.ty)
    } else {
        format!(
            "{hc} is {} here ({}), and joins {fc}, which is {}",
            h.ty, h.what, f.ty
        )
    };
    Diagnostic::error(h.span, msg)
        .with_label(f.span, format!("{fc} is {} here ({})", f.ty, f.what))
        .with_help("one column has one type: declare it `any` to take both")
}

/// The type two values of `a` and `b` given to one untyped cell are both
/// of: an enum and a string a string, two enums their members together,
/// an int and a float a number; `None` where they never meet.
pub(crate) fn wider(a: &Ty, b: &Ty) -> Option<Ty> {
    let string = || Some(Ty::Scalar("string".into()));
    match (a, b) {
        _ if a == b => Some(a.clone()),
        (Ty::Enum(x), Ty::Enum(y)) => {
            let mut m = x.clone();
            m.extend(y.iter().filter(|v| !x.contains(v)).cloned());
            Some(Ty::Enum(m))
        }
        (Ty::Scalar(s), t) | (t, Ty::Scalar(s)) if s == "string" && to_string(t) => string(),
        (Ty::Scalar(x), Ty::Scalar(y)) if number(x) && number(y) => {
            Some(Ty::Scalar("number".into()))
        }
        (Ty::Ref(x), Ty::Ref(y)) => Some(Ty::ref_union(x, y)),
        _ if compatible(a, b) => Some(narrower(a.clone(), b)),
        _ => None,
    }
}

/// A type whose values are text a string holds as they are: an enum's
/// members, a network, an address, an image reference, a version.
pub(crate) fn to_string(ty: &Ty) -> bool {
    match ty {
        Ty::Enum(_) => true,
        Ty::Scalar(s) => text_of("string", s),
        _ => false,
    }
}

/// `ty` with its article: `an int`, `a string`; an enum as it is written.
pub(crate) fn a(ty: &Ty) -> String {
    match ty {
        Ty::Scalar(s) if s.starts_with(['a', 'e', 'i', 'o']) => format!("an {ty}"),
        Ty::Scalar(_) | Ty::List(_) | Ty::Map(_) | Ty::Ref(_) | Ty::Secret(_) => format!("a {ty}"),
        _ => ty.to_string(),
    }
}

/// Why a value of `got` is not one of `want`: `an int is not a string`.
fn not_a(got: &Ty, want: &Ty) -> String {
    format!("{} is not {}", a(got), a(want))
}

/// A reference compared with a string (`written`, `read` the reference):
/// never equal, so an error, with the resource to write, or `has` where
/// the string is empty.
pub(crate) fn ref_and_string(
    written: &str,
    read: &str,
    want: &str,
    text: &str,
    span: Span,
) -> Diagnostic {
    let help = match (text.is_empty(), Ty::ref_types(want).next()) {
        (true, _) => format!("`has {read}` tests whether it is set"),
        (false, Some(typ)) if !want.contains('|') => format!(
            "compare with the resource: `{}`, or its name in scope",
            crate::address::Address {
                typ: typ.to_string(),
                name: text.to_string(),
            }
        ),
        _ => "compare with the resource: its name in scope, or `T[\"a\"]`".to_string(),
    };
    Diagnostic::error(
        span,
        format!(
            "{written} compares a reference, ref({want}), with the string {text:?}: a reference \
             is never a string"
        ),
    )
    .with_help(help)
}

pub(crate) fn shown(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("the string {s:?}"),
        Value::Int(i) => format!("the int {i}"),
        Value::Float(f) => format!("the float {f}"),
        Value::Bool(b) => format!("the bool {b}"),
        v => format!("`{}`", spell::value(v)),
    }
}
