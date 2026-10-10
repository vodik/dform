//! What a plan is (actions and their changes), and the documents both
//! sides of the provider protocol read: the engine and a provider compare
//! documents in one canonical form (`flatten`, `diff`), driven by the
//! provider's schema.

use crate::schema::Schema;
use crate::value::Value;
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub enum ActionKind {
    Create,
    Adopt,
    Update,
    /// An update against a stale identity (F DR-11 revised): a fresh null
    /// where the world holds a constant. Applied as an update.
    Drift,
    /// An update that waits on a null (`Action::on`) until a boundary.
    Pending,
    /// An update that changes a `force_new` path: the provider cannot
    /// update in place. The old object is deleted before the new one is
    /// created, or after (`create_first`: the type's `type_replace`, or
    /// `lifecycle(r, "create_first")`), when it stays deposed in state
    /// until then.
    Replace {
        create_first: bool,
    },
    Delete,
    /// Delete the object a create-first replacement deposed.
    DeleteDeposed,
    /// A delete of an object `lifecycle(r, "retain")` keeps (R-154): no
    /// provider call; state drops it and the world keeps it.
    Forget,
    Noop,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    /// The schema marks the path sensitive: print neither side.
    pub sensitive: bool,
    /// What the plan says of it beside its line, where dform can tell
    /// more than the two sides: of a value given at creation only that is
    /// kept (R-198) and holds a secret a provider holds, which part
    /// differs, [`HOLDER_REPLACED`] or [`TEMPLATE_CHANGED`]. dform's own;
    /// never on the wire.
    pub note: Option<String>,
}

/// A kept value's note (R-218): the object holding a secret it reveals
/// is another one (a key replaced), the value around it is the same.
pub const HOLDER_REPLACED: &str = "(the key was replaced)";

/// A kept value's note (R-218): the value around the secrets it reveals
/// changed.
pub const TEMPLATE_CHANGED: &str = "(the template changed)";

/// How a labeled null (proposal E §2.2) travels in a provider document: an
/// object with the single key `$null` holding the label `type/addr#attr`.
/// Plan output prints it `?type/addr#attr`.
pub const NULL_KEY: &str = "$null";
/// A secret travels as its label only, never its bytes: `{"$secret": label}`.
/// The provider materializes it inside Apply; output prints it redacted.
pub const SECRET_KEY: &str = "$secret";

pub fn null_json(label: &str) -> serde_json::Value {
    serde_json::json!({ NULL_KEY: label })
}

pub fn secret_json(label: &str) -> serde_json::Value {
    serde_json::json!({ SECRET_KEY: label })
}

/// A secret marker's second key, when another stack's secret is read
/// (`stack_output`): where a provider holds it, a [`Held`].
pub const HELD_KEY: &str = "held";

/// Where a provider holds a secret another stack reads (E DR-19): the
/// object `remote` of type `typ` that the provider `provider` manages for
/// the deployment `deployment`, at `path`. The reading stack's provider
/// reads the bytes there inside Apply; they never pass through dform or
/// its stores. `digest` is the keyed digest of the value when the
/// deployment that manages it knew the value (a configured attribute), so
/// a change to it changes the reader's document; empty when it never did
/// (a sensitive computed value).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Held {
    pub provider: String,
    pub deployment: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub remote: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
}

/// A secret marker that says where its value is held:
/// `{"$secret": label, "held": {..}}`.
pub fn held_json(label: &str, held: &Held) -> serde_json::Value {
    serde_json::json!({
        SECRET_KEY: label,
        HELD_KEY: serde_json::to_value(held).expect("a held secret serializes"),
    })
}

/// A null or secret marker's key and label.
pub fn marker(v: &serde_json::Value) -> Option<(&'static str, &str)> {
    let m = v.as_object()?;
    match m.len() {
        1 => {}
        2 if m.contains_key(SECRET_KEY) && m.contains_key(HELD_KEY) => {}
        _ => return None,
    }
    for k in [NULL_KEY, SECRET_KEY] {
        if let Some(serde_json::Value::String(l)) = m.get(k) {
            return Some((k, l));
        }
    }
    None
}

/// Where a secret marker's value is held, when it says.
pub fn held(v: &serde_json::Value) -> Option<Held> {
    marker(v)?;
    serde_json::from_value(v.get(HELD_KEY)?.clone()).ok()
}

/// Render one side of a change for plan output.
pub fn fmt_value(v: Option<&serde_json::Value>) -> String {
    let Some(v) = v else {
        return "<none>".to_string();
    };
    match marker(v) {
        Some((NULL_KEY, l)) => return format!("?{}", crate::address::label(l)),
        Some((_, l)) => return format!("(sensitive {})", crate::address::label(l)),
        None => {}
    }
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<unprintable>".to_string()),
    }
}

#[derive(Debug, Clone)]
pub struct Action {
    pub kind: ActionKind,
    pub addr: crate::address::Address,
    pub changes: Vec<Change>,
    /// Pending: the nulls the comparison waits on.
    pub on: std::collections::BTreeSet<String>,
    /// Of an object that exists, each attribute given at creation only
    /// (`lifecycle(r, "bootstrap", P)`) whose value differs from what it
    /// was made with: no change, kept, and said (R-198). A replace makes
    /// the object again, so it sends them: changes that force nothing
    /// ([`Action::sent`]).
    pub kept: Vec<Change>,
    /// A create-first replace whose replacement dform names (R-189): the
    /// old object's remote name and the new one's, the next generation of
    /// the program's (`forgejo-3`, `forgejo-4`).
    pub renamed: Option<(String, String)>,
}

impl Action {
    /// What the action sends that differs from the world: its changes,
    /// and a replace's values given at creation only, in path order.
    pub fn sent(&self) -> Vec<&Change> {
        let mut out: Vec<&Change> = self.changes.iter().collect();
        if let ActionKind::Replace { .. } = self.kind {
            out.extend(&self.kept);
            out.sort_by(|a, b| a.path.cmp(&b.path));
        }
        out
    }

    /// The values given at creation only it keeps: none of a replace's.
    pub fn kept(&self) -> &[Change] {
        match self.kind {
            ActionKind::Replace { .. } => &[],
            _ => &self.kept,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub actions: Vec<Action>,
}

/// A world value (or any JSON document) as the evaluator's value: a
/// number an integer or a float (R-75), `null` the string (the value model
/// has none).
pub fn json_to_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Str("null".into()),
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| {
                n.as_f64()
                    .and_then(crate::value::Float::new)
                    .map(Value::Float)
            })
            .unwrap_or(Value::Str(n.to_string())),
        Json::String(s) => Value::Str(s.clone()),
        Json::Array(xs) => Value::List(xs.iter().map(json_to_value).collect()),
        Json::Object(m) => Value::Obj(
            m.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

pub fn short_hash(s: &str) -> String {
    // FNV-1a, printed base 36: deterministic across runs and platforms.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::new();
    for _ in 0..5 {
        out.push(digits[(h % 36) as usize] as char);
        h /= 36;
    }
    out
}

/// Remove a stored path (a quoted segment is one key, R-77); an object
/// left empty by it goes too.
pub fn remove_path(v: &mut Json, path: &str) {
    let Some(m) = v.as_object_mut() else {
        return;
    };
    match crate::address::path_split_first(path) {
        None => {
            m.remove(crate::address::segment_key(path).as_ref());
        }
        Some((head, rest)) => {
            let head = crate::address::segment_key(head);
            if let Some(child) = m.get_mut(head.as_ref()) {
                remove_path(child, rest);
                if child.as_object().is_some_and(|c| c.is_empty()) {
                    m.remove(head.as_ref());
                }
            }
        }
    }
}

pub fn set_path(v: &mut Json, path: &str, x: Json) {
    let mut cur = v;
    let keys = crate::address::path_keys(path);
    let mut parts = keys.into_iter().peekable();
    while let Some(p) = parts.next() {
        if !cur.is_object() {
            *cur = json!({});
        }
        let m = cur.as_object_mut().unwrap();
        if parts.peek().is_none() {
            m.insert(p, x);
            return;
        }
        cur = m.entry(p).or_insert_with(|| json!({}));
    }
}

/// The value at a keypath (`tags.owner`, `subnets[0].id`, `labels."a.b"`)
/// in nested JSON, the shape `set_path` builds and `flatten`
/// spells.
pub fn get_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in crate::address::path_segments(path) {
        let (key, mut rest) = crate::address::segment_parts(seg);
        if !key.is_empty() {
            cur = cur.get(key.as_ref())?;
        }
        while let Some(r) = rest.strip_prefix('[') {
            let (idx, tail) = r.split_once(']')?;
            cur = cur.get(idx.parse::<usize>().ok()?)?;
            rest = tail;
        }
        if !rest.is_empty() {
            return None;
        }
    }
    Some(cur)
}

/// A path as the schema spells it: no list indices or keys
/// (`containers[name=api].image` is `containers.image`).
pub fn norm_path(path: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in path.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// The leaf-by-leaf changes from `before` to `after`, in the plan's
/// canonical form (keyed lists by key, sets as sets).
pub fn diff(
    schema: &Schema,
    typ: &str,
    before: Option<&Json>,
    after: Option<&Json>,
) -> Vec<Change> {
    let mut a = BTreeMap::new();
    let mut b = BTreeMap::new();
    // A set's elements are matched by content between two documents; a
    // create's or a delete's are by their place in the one there is, as
    // the program's value holds them.
    let both = before.is_some() && after.is_some();
    if let Some(v) = before {
        flatten(schema, typ, v, "", "", both, &mut a);
    }
    if let Some(v) = after {
        flatten(schema, typ, v, "", "", both, &mut b);
    }
    let paths: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let mut out = Vec::new();
    for p in paths {
        let (av, bv) = (a.get(p), b.get(p));
        if av.map(|x| &x.0) == bv.map(|x| &x.0) {
            continue;
        }
        let norm = av.or(bv).map(|x| x.1.as_str()).unwrap_or("");
        out.push(Change {
            path: p.clone(),
            before: av.map(|x| x.0.clone()),
            after: bv.map(|x| x.0.clone()),
            sensitive: schema.is_sensitive(typ, norm),
            note: None,
        });
    }
    out
}

/// Flatten a document to leaf paths. `norm` is the schema path (dotted, no
/// indices). A list with `type_list_key` merge keys is spelled by key,
/// `containers[name=web]`, so reordering is not a change; a `set` is
/// compared as a set. Null and secret markers are leaves. `by_content`
/// labels an element of a keyless set by a hash of its content,
/// `ingress[#k3j2d]`, so a diff shows an element added or removed rather
/// than every later index shifting; otherwise by its position in `v`.
/// An empty object is a leaf where the schema says `{}` is a value
/// (`Schema::empty_is_present`); elsewhere it has no leaf, as absent.
pub fn flatten(
    schema: &Schema,
    typ: &str,
    v: &Json,
    prefix: &str,
    norm: &str,
    by_content: bool,
    out: &mut BTreeMap<String, (Json, String)>,
) {
    if marker(v).is_some() {
        out.insert(prefix.to_string(), (v.clone(), norm.to_string()));
        return;
    }
    match v {
        Json::Object(m) if m.is_empty() && !norm.is_empty() => {
            if schema.empty_is_present(typ, norm) {
                out.insert(prefix.to_string(), (v.clone(), norm.to_string()));
            }
        }
        Json::Object(m) => {
            for (k, vv) in m {
                // A key holding `.`, `[`, `]` or `/` is its segment quoted
                // (R-77): `metadata.annotations."a.b/c"`.
                let join = |p: &str| crate::address::path_join(p, k);
                flatten(schema, typ, vv, &join(prefix), &join(norm), by_content, out);
            }
        }
        Json::Array(xs) => {
            let keys = schema.list_key(typ, norm);
            let is_set = schema.attr(typ, norm).is_some_and(|a| a.kind() == "set");
            let mut items: Vec<(String, &Json)> = Vec::new();
            for (i, vv) in xs.iter().enumerate() {
                let by_key =
                    keys.and_then(|ks| crate::report::fold::label(ks, &key_fields(ks, vv)));
                let label = match by_key {
                    Some(label) => label,
                    None if is_set => serde_json::to_string(vv).unwrap_or_default(),
                    None => i.to_string(),
                };
                items.push((label, vv));
            }
            if is_set && keys.is_none() {
                if by_content {
                    items.sort_by(|x, y| x.0.cmp(&y.0));
                }
                for (i, it) in items.iter_mut().enumerate() {
                    it.0 = if by_content {
                        format!("#{}", short_hash(&it.0))
                    } else {
                        i.to_string()
                    };
                }
            }
            for (label, vv) in items {
                flatten(
                    schema,
                    typ,
                    vv,
                    &format!("{prefix}[{label}]"),
                    norm,
                    by_content,
                    out,
                );
            }
        }
        _ => {
            out.insert(prefix.to_string(), (v.clone(), norm.to_string()));
        }
    }
}

/// Element `x`'s fields `keys` as the evaluator's value, to label it by
/// (`report::fold::label`): an unknown or a secret as its label.
fn key_fields(keys: &[String], x: &Json) -> Value {
    let field = |x: &Json| match marker(x) {
        Some(_) => Value::Str(fmt_value(Some(x))),
        None => json_to_value(x),
    };
    Value::Obj(
        keys.iter()
            .filter_map(|k| Some((k.clone(), field(x.get(k)?))))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_object_is_a_leaf_only_where_the_schema_says_it_is_a_value() {
        let schema = Schema::parse(
            r#"
type_attr(k8s.np, "spec.podSelector", "object", [])
type_attr(k8s.np, "spec.podSelector.matchLabels", "map", [])
type_attr(k8s.np, "spec.podSelector.matchExpressions", "list", [])
type_attr(k8s.np, "spec.podSelector.matchExpressions.key", "string", ["required"])
type_attr(k8s.np, "metadata.labels", "map", [])
type_attr(k8s.np, "spec.ref", "object", [])
type_attr(k8s.np, "spec.ref.name", "string", ["required"])
"#,
            "test",
        )
        .unwrap();
        let doc = json!({"spec": {"podSelector": {}, "ref": {}}, "metadata": {"labels": {}}});
        let mut out = BTreeMap::new();
        flatten(&schema, "k8s.np", &doc, "", "", true, &mut out);
        assert_eq!(out.keys().collect::<Vec<_>>(), ["spec.podSelector"]);
        // Absent and {} differ where it is a value: adding it is a change.
        let changes = diff(
            &schema,
            "k8s.np",
            Some(&json!({"spec": {}})),
            Some(&json!({"spec": {"podSelector": {}}})),
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "spec.podSelector");
        assert_eq!(changes[0].after, Some(json!({})));
        // A key under it is the leaf instead.
        let mut out = BTreeMap::new();
        let doc = json!({"spec": {"podSelector": {"matchLabels": {"a": "b"}}}});
        flatten(&schema, "k8s.np", &doc, "", "", true, &mut out);
        assert_eq!(
            out.keys().collect::<Vec<_>>(),
            ["spec.podSelector.matchLabels.a"]
        );
    }

    #[test]
    fn get_path_walks_dots_and_indices() {
        let v = json!({"tags": {"owner": "team-a"}, "subnets": [{"id": "s-0"}, {"id": "s-1"}], "id": "x"});
        assert_eq!(get_path(&v, "id"), Some(&json!("x")));
        assert_eq!(get_path(&v, "tags.owner"), Some(&json!("team-a")));
        assert_eq!(get_path(&v, "subnets[1].id"), Some(&json!("s-1")));
        assert_eq!(get_path(&v, "subnets[2].id"), None);
        assert_eq!(get_path(&v, "tags.missing"), None);
    }

    #[test]
    fn norm_path_drops_list_indices_and_keys() {
        assert_eq!(norm_path("containers[name=api].image"), "containers.image");
        assert_eq!(norm_path("subnet_ids[0]"), "subnet_ids");
        assert_eq!(norm_path("tags.team"), "tags.team");
    }
}
