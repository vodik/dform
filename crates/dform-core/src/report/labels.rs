//! How the report names things (R-111, R-112): a resource's address, a reference
//! and an attribute, a null's or a secret's label as what it stands for, a stored
//! name as the source names it, a relation, a change's kind and mark, an id.

use crate::ir::Address;
use crate::provider::ActionKind;

/// A resource's address as the plan, `why`, `query` and the editor print
/// it (R-111): its type, then its path the way the source names it,
/// `ovh.ssh_key k3s.admin`. The full `T["k3s.admin"]` is for the plan
/// file, `--json`, state, and an argument.
pub fn address(a: &Address) -> String {
    format!("{} {}", a.typ, path(&a.name))
}

/// A stored name as the source names it (R-112): its path, a copy's scope
/// before it (`k3s.admin`), a segment holding a dot already quoted
/// (`k3s."k8s-lab.vodik.xyz"`); a segment holding a space quoted too, and
/// an empty name `""`, so the printed path reads back as one.
pub fn path(name: &str) -> String {
    if name.is_empty() {
        return "\"\"".into();
    }
    crate::ir::path_segments(name)
        .into_iter()
        .map(|seg| {
            let bare = !seg.starts_with('"')
                && seg.contains(|c: char| c.is_whitespace() || c.is_control());
            match bare {
                true => crate::ir::string_literal(seg),
                false => seg.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A reference to resource `a`, or to its attribute `attr`, as a value
/// prints: its path, `k3s.server`, `k3s.server.public_ip`.
pub fn reference(a: &Address, attr: &str) -> String {
    format!("{}{}", path(&a.name), crate::ir::path_suffix(attr))
}

/// A resource's attribute as a diagnostic names it: `T k3s.server.p`;
/// an input's by its path, `input db.backup_days`.
pub fn attribute(a: &Address, attr: &str) -> String {
    match a.name.is_empty() && !attr.is_empty() {
        true => format!("{} {attr}", a.typ),
        false => format!("{} {}", a.typ, reference(a, attr)),
    }
}

/// A null's or a secret's label `T/A#P` as a diagnostic names what it
/// stands for: [`attribute`], `T k3s.server.public_ip`; a resource's
/// identity [`address`]; an input's or an output's as
/// [`crate::ir::label`] says it.
pub fn attribute_label(l: &str) -> String {
    if let Some(call) = extern_label(l) {
        return call;
    }
    match crate::value::null_parts(l) {
        Some((typ, name, _)) if typ == crate::stack::UNAPPLIED => format!("stack {name}"),
        Some((typ, name, p)) if !name.is_empty() && typ != crate::transform::OUTPUT => {
            let a = Address { typ, name };
            match p == crate::schema::IDENTITY {
                true => address(&a),
                false => attribute(&a, &p),
            }
        }
        _ => crate::ir::label(l),
    }
}

/// A null's or a secret's label `T/A#P` as the value it stands for
/// (R-111): the reference it is, `k3s.server.public_ip`; a resource's
/// identity the resource, `k3s.server`; an input's or an output's as
/// [`crate::ir::label`] says it.
pub fn label(l: &str) -> String {
    if let Some(call) = extern_label(l) {
        return call;
    }
    match crate::value::null_parts(l) {
        // What reads a deployment not applied yet waits on it (R-121).
        Some((typ, name, _)) if typ == crate::stack::UNAPPLIED => format!("stack {name}"),
        Some((typ, name, p)) if !name.is_empty() && typ != crate::transform::OUTPUT => {
            let a = Address { typ, name };
            match p == crate::schema::IDENTITY {
                true => reference(&a, ""),
                false => reference(&a, &p),
            }
        }
        _ => crate::ir::label(l),
    }
}

/// The label of an answer of dform's own extern (`ssh.read/INPUTS#N`,
/// what a host still booting has "not yet" said) as its call:
/// `ssh.read("127.0.0.1:22", "ubuntu", "/etc/k3s.yaml")`. Its inputs are
/// no address, so never split at their dots.
pub(super) fn extern_label(l: &str) -> Option<String> {
    let (pred, inputs, col) = crate::value::null_parts(l)?;
    crate::externs::is_call_label(&pred, &col).then(|| crate::externs::call_text(&pred, &inputs))
}

/// A label as [`crate::ir::label`] printed it (`T["A"].p`), when it is an
/// extern call's ([`extern_label`]).
fn printed_call(l: &str) -> Option<String> {
    let (typ, rest) = l.split_once('[')?;
    let (inputs, col) = rest.rsplit_once("].")?;
    let inputs = crate::syntax::resolve::unescape(inputs).ok()?;
    let col = col.trim_matches('"');
    crate::externs::is_call_label(typ, col).then(|| crate::externs::call_text(typ, &inputs))
}

/// A label as [`crate::ir::label`] printed it (`T["A"].p`), as
/// [`label`] prints it.
pub(super) fn printed_label(l: &str) -> String {
    if let Some(call) = printed_call(l) {
        return call;
    }
    match crate::ir::parse_address(l) {
        Ok((a, p)) => reference(&a, p.as_deref().unwrap_or_default()),
        Err(_) => l.to_string(),
    }
}

/// A label as [`crate::ir::label`] printed it (`T["A"].p`), as
/// [`attribute_label`] prints it; a call (`random.password("db")`) as
/// itself.
pub(super) fn printed_attribute(l: &str) -> String {
    if let Some(call) = printed_call(l) {
        return call;
    }
    match crate::ir::parse_address(l) {
        Ok((a, p)) => attribute(&a, p.as_deref().unwrap_or_default()),
        Err(_) => l.to_string(),
    }
}

/// An address the report holds as text (`T["A"]`, a pending group's
/// `T[?]` or `T["name-${x}"]`) as [`address`] prints it: `T ?` for an
/// unknown number, a template as the statement writes it.
pub fn address_text(s: &str) -> String {
    if let Ok(a) = crate::ir::parse_resource_address(s) {
        return address(&a);
    }
    match s.split_once('[') {
        Some((t, rest)) if rest.ends_with(']') && !t.is_empty() => {
            format!("{t} {}", &rest[..rest.len() - 1])
        }
        _ => s.to_string(),
    }
}

/// An id as messages print it (a commit, a master's): its first 12
/// characters.
pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// The mark a deformation of kind `k` has in the plan: `+`, `~`, `-`.
pub fn marker_of(k: &ActionKind) -> &'static str {
    match k {
        ActionKind::Create => "+",
        ActionKind::Adopt => ">",
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending | ActionKind::Forget => "~",
        ActionKind::Delete | ActionKind::DeleteDeposed => "-",
        ActionKind::Replace { .. } => "±",
        ActionKind::Noop => "=",
    }
}

/// A deformation's kind as the plan names it: `create`, `update`.
pub fn kind_name(k: &ActionKind) -> &'static str {
    match k {
        ActionKind::Create => "create",
        ActionKind::Adopt => "adopt",
        ActionKind::Update => "update",
        ActionKind::Drift => "drift",
        ActionKind::Pending => "update",
        ActionKind::Replace { .. } => "replace",
        ActionKind::Delete | ActionKind::DeleteDeposed => "delete",
        ActionKind::Forget => "forget",
        ActionKind::Noop => "no-op",
    }
}

/// A relation as the source names it (R-111): a copy's or a used
/// module's own by its name there, `vpc_net (in green)`, never the core's
/// `green::vpc_net`.
pub fn relation_name(rel: &str) -> String {
    match rel.rsplit_once("::") {
        Some((scope, p)) => format!("{p} (in {scope})"),
        None => rel.to_string(),
    }
}
