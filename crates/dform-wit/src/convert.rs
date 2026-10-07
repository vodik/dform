//! The protocol's messages (`dform-wire`, prost) as the WIT's types and
//! back, written once for both sides: [`convert!`] expands it over a
//! bindings module (`dform:provider/types` as wasmtime or wit-bindgen
//! generated it; the two name every type and field alike), so the host
//! (`dform-host`) and the guest (`dform-sdk`) convert the same way.
//!
//! The encodings differ as `wit/dform-provider.wit`'s header records: a
//! value travels as a `tree` (its nodes in a list, children by index, each
//! child after its parent); an absent document is the empty obj; an enum
//! has no `*_UNSPECIFIED` case (one read from the proto is the first case:
//! dform never sends it, and the host refuses an Apply without an op
//! before converting).

/// Expand the conversions over the bindings module `$w` (the path of
/// `dform::provider::types`) into a module named `$m`. Each message has
/// `to_X` (prost to WIT, total) and `from_X` (WIT to prost, which fails on
/// a malformed tree). The caller depends on `dform-wire`.
#[macro_export]
macro_rules! convert {
    ($m:ident, $w:path) => {
        #[allow(dead_code, clippy::all)]
        pub mod $m {
            use ::dform_wire as pb;
            use $w as w;

            type R<T> = ::std::result::Result<T, String>;

            fn push(nodes: &mut Vec<w::Value>, v: &pb::Value) -> u32 {
                use pb::value::Kind as K;
                let at = nodes.len();
                nodes.push(w::Value::Bool(false));
                let node = match &v.kind {
                    None => w::Value::Null(w::Null {
                        label: String::new(),
                        class: w::NullClass::Open,
                        ty: String::new(),
                        held: None,
                    }),
                    Some(K::Str(s)) => w::Value::Str(s.clone()),
                    Some(K::Int(i)) => w::Value::Int(*i),
                    Some(K::Bool(b)) => w::Value::Bool(*b),
                    Some(K::List(l)) => {
                        let items = l.items.iter().map(|x| push(nodes, x)).collect();
                        w::Value::List(w::List { items })
                    }
                    Some(K::Obj(o)) => {
                        let fields = o
                            .fields
                            .iter()
                            .map(|(k, x)| (k.clone(), push(nodes, x)))
                            .collect();
                        w::Value::Obj(w::Obj { fields })
                    }
                    Some(K::Ip(a)) => w::Value::Ip(*a),
                    Some(K::IpNet(n)) => w::Value::IpNet(w::IpNet {
                        addr: n.addr,
                        prefix: n.prefix,
                    }),
                    Some(K::IpRange(r)) => w::Value::IpRange(w::IpRange {
                        start: r.start,
                        end: r.end,
                    }),
                    Some(K::Ref(r)) => w::Value::Ref(to_ref(r)),
                    Some(K::CloudRef(r)) => w::Value::CloudRef(to_ref(r)),
                    Some(K::Null(n)) => w::Value::Null(w::Null {
                        label: n.label.clone(),
                        class: match pb::NullClass::try_from(n.class) {
                            Ok(pb::NullClass::Fresh) => w::NullClass::Fresh,
                            Ok(pb::NullClass::Secret) => w::NullClass::Secret,
                            _ => w::NullClass::Open,
                        },
                        ty: n.ty.clone(),
                        held: n.held.as_ref().map(to_held),
                    }),
                    Some(K::Float(f)) => w::Value::Float(*f),
                };
                nodes[at] = node;
                at as u32
            }

            fn to_ref(r: &pb::Ref) -> w::Ref {
                w::Ref {
                    type_: r.r#type.clone(),
                    name: r.name.clone(),
                    attr: r.attr.clone(),
                }
            }

            fn from_ref(r: &w::Ref) -> pb::Ref {
                pb::Ref {
                    r#type: r.type_.clone(),
                    name: r.name.clone(),
                    attr: r.attr.clone(),
                }
            }

            pub fn to_tree(v: &pb::Value) -> w::Tree {
                let mut nodes = Vec::new();
                push(&mut nodes, v);
                w::Tree { nodes }
            }

            /// An absent document is the empty obj.
            pub fn to_doc(v: &Option<pb::Value>) -> w::Tree {
                match v {
                    Some(v) => to_tree(v),
                    None => w::Tree {
                        nodes: vec![w::Value::Obj(w::Obj { fields: Vec::new() })],
                    },
                }
            }

            fn node(nodes: &[w::Value], at: u32, parent: Option<u32>) -> R<pb::Value> {
                use pb::value::Kind as K;
                if parent.is_some_and(|p| at <= p) {
                    return Err(format!("a tree's node {at} is not after its parent"));
                }
                let v = nodes
                    .get(at as usize)
                    .ok_or_else(|| format!("a tree has no node {at}"))?;
                let kind = match v {
                    w::Value::Str(s) => K::Str(s.clone()),
                    w::Value::Int(i) => K::Int(*i),
                    w::Value::Bool(b) => K::Bool(*b),
                    w::Value::List(l) => K::List(pb::List {
                        items: l
                            .items
                            .iter()
                            .map(|i| node(nodes, *i, Some(at)))
                            .collect::<R<_>>()?,
                    }),
                    w::Value::Obj(o) => K::Obj(pb::Obj {
                        fields: o
                            .fields
                            .iter()
                            .map(|(k, i)| Ok((k.clone(), node(nodes, *i, Some(at))?)))
                            .collect::<R<_>>()?,
                    }),
                    w::Value::Ip(a) => K::Ip(*a),
                    w::Value::IpNet(n) => K::IpNet(pb::IpNet {
                        addr: n.addr,
                        prefix: n.prefix,
                    }),
                    w::Value::IpRange(r) => K::IpRange(pb::IpRange {
                        start: r.start,
                        end: r.end,
                    }),
                    w::Value::Ref(r) => K::Ref(from_ref(r)),
                    w::Value::CloudRef(r) => K::CloudRef(from_ref(r)),
                    w::Value::Null(n) => K::Null(pb::Null {
                        label: n.label.clone(),
                        class: match n.class {
                            w::NullClass::Fresh => pb::NullClass::Fresh,
                            w::NullClass::Open => pb::NullClass::Open,
                            w::NullClass::Secret => pb::NullClass::Secret,
                        } as i32,
                        ty: n.ty.clone(),
                        held: n.held.as_ref().map(from_held),
                    }),
                    w::Value::Float(f) => K::Float(*f),
                };
                Ok(pb::Value { kind: Some(kind) })
            }

            pub fn from_tree(t: &w::Tree) -> R<pb::Value> {
                node(&t.nodes, 0, None)
            }

            fn from_doc(t: &w::Tree) -> R<Option<pb::Value>> {
                from_tree(t).map(Some)
            }

            fn trees(v: &[pb::Value]) -> Vec<w::Tree> {
                v.iter().map(to_tree).collect()
            }

            fn from_trees(v: &[w::Tree]) -> R<Vec<pb::Value>> {
                v.iter().map(from_tree).collect()
            }

            pub fn to_call_error(e: &::dform_core::plugin::backend::CallError) -> w::CallError {
                use ::dform_core::plugin::backend::CallError as E;
                match e {
                    E::Refused(m) => w::CallError::Refused(m.clone()),
                    E::MaybeApplied(m) => w::CallError::MaybeApplied(m.clone()),
                    E::Crashed(m) => w::CallError::Crashed(m.clone()),
                }
            }

            pub fn from_call_error(e: w::CallError) -> ::dform_core::plugin::backend::CallError {
                use ::dform_core::plugin::backend::CallError as E;
                match e {
                    w::CallError::Refused(m) => E::Refused(m),
                    w::CallError::MaybeApplied(m) => E::MaybeApplied(m),
                    w::CallError::Crashed(m) => E::Crashed(m),
                }
            }

            fn to_fact(f: &pb::Fact) -> w::Fact {
                w::Fact {
                    pred: f.pred.clone(),
                    args: trees(&f.args),
                }
            }

            fn from_fact(f: &w::Fact) -> R<pb::Fact> {
                Ok(pb::Fact {
                    pred: f.pred.clone(),
                    args: from_trees(&f.args)?,
                })
            }

            pub fn to_handshake_request(r: &pb::HandshakeRequest) -> w::HandshakeRequest {
                w::HandshakeRequest {
                    protocol_version: r.protocol_version,
                }
            }

            pub fn from_handshake_request(r: &w::HandshakeRequest) -> R<pb::HandshakeRequest> {
                Ok(pb::HandshakeRequest {
                    protocol_version: r.protocol_version,
                })
            }

            pub fn to_handshake_response(r: &pb::HandshakeResponse) -> w::HandshakeResponse {
                w::HandshakeResponse {
                    protocol_version: r.protocol_version,
                    name: r.name.clone(),
                    capabilities: r.capabilities.clone(),
                    version: r.version.clone(),
                }
            }

            pub fn from_handshake_response(r: &w::HandshakeResponse) -> R<pb::HandshakeResponse> {
                Ok(pb::HandshakeResponse {
                    protocol_version: r.protocol_version,
                    name: r.name.clone(),
                    capabilities: r.capabilities.clone(),
                    version: r.version.clone(),
                })
            }

            pub fn to_configure_request(r: &pb::ConfigureRequest) -> w::ConfigureRequest {
                w::ConfigureRequest {
                    config: to_doc(&r.config),
                }
            }

            pub fn from_configure_request(r: &w::ConfigureRequest) -> R<pb::ConfigureRequest> {
                Ok(pb::ConfigureRequest {
                    config: from_doc(&r.config)?,
                })
            }

            pub fn to_configure_response(r: &pb::ConfigureResponse) -> w::ConfigureResponse {
                w::ConfigureResponse {
                    account: r.account.clone(),
                }
            }

            pub fn from_configure_response(r: &w::ConfigureResponse) -> R<pb::ConfigureResponse> {
                Ok(pb::ConfigureResponse {
                    account: r.account.clone(),
                })
            }

            pub fn to_schema_request(r: &pb::SchemaRequest) -> w::SchemaRequest {
                w::SchemaRequest {
                    types: r.types.as_ref().map(|t| w::TypeFilter {
                        names: t.names.clone(),
                    }),
                }
            }

            pub fn from_schema_request(r: &w::SchemaRequest) -> R<pb::SchemaRequest> {
                Ok(pb::SchemaRequest {
                    types: r.types.as_ref().map(|t| pb::TypeFilter {
                        names: t.names.clone(),
                    }),
                })
            }

            pub fn to_schema_response(r: &pb::SchemaResponse) -> w::SchemaResponse {
                w::SchemaResponse {
                    facts: r.facts.iter().map(to_fact).collect(),
                    externs: r
                        .externs
                        .iter()
                        .map(|e| w::ExternDecl {
                            pred: e.pred.clone(),
                            arity: e.arity,
                            input: e.input.clone(),
                        })
                        .collect(),
                    checks_refinements: r.checks_refinements,
                    examples: r
                        .examples
                        .iter()
                        .map(|e| w::Example {
                            type_: e.r#type.clone(),
                            create: to_doc(&e.create),
                            update: to_doc(&e.update),
                            required: e.required.clone(),
                        })
                        .collect(),
                }
            }

            pub fn from_schema_response(r: &w::SchemaResponse) -> R<pb::SchemaResponse> {
                Ok(pb::SchemaResponse {
                    facts: r.facts.iter().map(from_fact).collect::<R<_>>()?,
                    externs: r
                        .externs
                        .iter()
                        .map(|e| pb::ExternDecl {
                            pred: e.pred.clone(),
                            arity: e.arity,
                            input: e.input.clone(),
                        })
                        .collect(),
                    checks_refinements: r.checks_refinements,
                    examples: r
                        .examples
                        .iter()
                        .map(|e| {
                            Ok(pb::Example {
                                r#type: e.type_.clone(),
                                create: from_doc(&e.create)?,
                                update: from_doc(&e.update)?,
                                required: e.required.clone(),
                            })
                        })
                        .collect::<R<_>>()?,
                })
            }

            pub fn to_query_request(r: &pb::QueryRequest) -> w::QueryRequest {
                w::QueryRequest {
                    pred: r.pred.clone(),
                    input: r.input.clone(),
                    inputs: trees(&r.inputs),
                    secret: r.secret.clone(),
                }
            }

            pub fn from_query_request(r: &w::QueryRequest) -> R<pb::QueryRequest> {
                Ok(pb::QueryRequest {
                    pred: r.pred.clone(),
                    input: r.input.clone(),
                    inputs: from_trees(&r.inputs)?,
                    secret: r.secret.clone(),
                })
            }

            pub fn to_rows(rows: &[pb::Row]) -> Vec<w::Row> {
                rows.iter()
                    .map(|r| w::Row {
                        values: trees(&r.values),
                    })
                    .collect()
            }

            pub fn from_rows(rows: &[w::Row]) -> R<Vec<pb::Row>> {
                rows.iter()
                    .map(|r| {
                        Ok(pb::Row {
                            values: from_trees(&r.values)?,
                        })
                    })
                    .collect()
            }

            pub fn to_read_request(r: &pb::ReadRequest) -> w::ReadRequest {
                w::ReadRequest {
                    type_: r.r#type.clone(),
                    remote: r.remote.clone(),
                    name: r.name.clone(),
                }
            }

            pub fn from_read_request(r: &w::ReadRequest) -> R<pb::ReadRequest> {
                Ok(pb::ReadRequest {
                    r#type: r.type_.clone(),
                    remote: r.remote.clone(),
                    name: r.name.clone(),
                })
            }

            pub fn to_read_response(r: &pb::ReadResponse) -> w::ReadResponse {
                w::ReadResponse {
                    found: r.found,
                    attrs: to_doc(&r.attrs),
                    computed: to_doc(&r.computed),
                }
            }

            pub fn from_read_response(r: &w::ReadResponse) -> R<pb::ReadResponse> {
                Ok(pb::ReadResponse {
                    found: r.found,
                    attrs: from_doc(&r.attrs)?,
                    computed: from_doc(&r.computed)?,
                })
            }

            pub fn to_plan_request(r: &pb::PlanRequest) -> w::PlanRequest {
                w::PlanRequest {
                    type_: r.r#type.clone(),
                    name: r.name.clone(),
                    prior: r.prior.as_ref().map(to_tree),
                    desired: r.desired.as_ref().map(to_tree),
                    remote: r.remote.clone(),
                }
            }

            pub fn from_plan_request(r: &w::PlanRequest) -> R<pb::PlanRequest> {
                Ok(pb::PlanRequest {
                    r#type: r.type_.clone(),
                    name: r.name.clone(),
                    prior: r.prior.as_ref().map(from_tree).transpose()?,
                    desired: r.desired.as_ref().map(from_tree).transpose()?,
                    remote: r.remote.clone(),
                })
            }

            pub fn to_plan_response(r: &pb::PlanResponse) -> w::PlanResponse {
                w::PlanResponse {
                    changes: r
                        .changes
                        .iter()
                        .map(|c| w::Change {
                            path: c.path.clone(),
                            before: c.before.as_ref().map(to_tree),
                            after: c.after.as_ref().map(to_tree),
                            sensitive: c.sensitive,
                        })
                        .collect(),
                    requires_replace: r.requires_replace,
                }
            }

            pub fn from_plan_response(r: &w::PlanResponse) -> R<pb::PlanResponse> {
                Ok(pb::PlanResponse {
                    changes: r
                        .changes
                        .iter()
                        .map(|c| {
                            Ok(pb::Change {
                                path: c.path.clone(),
                                before: c.before.as_ref().map(from_tree).transpose()?,
                                after: c.after.as_ref().map(from_tree).transpose()?,
                                sensitive: c.sensitive,
                            })
                        })
                        .collect::<R<_>>()?,
                    requires_replace: r.requires_replace,
                })
            }

            fn to_op(op: i32) -> w::Op {
                match pb::Op::try_from(op) {
                    Ok(pb::Op::Update) => w::Op::Update,
                    Ok(pb::Op::Delete) => w::Op::Delete,
                    Ok(pb::Op::Adopt) => w::Op::Adopt,
                    Ok(pb::Op::Replace) => w::Op::Replace,
                    Ok(pb::Op::EndTick) => w::Op::EndTick,
                    _ => w::Op::Create,
                }
            }

            fn from_op(op: w::Op) -> pb::Op {
                match op {
                    w::Op::Create => pb::Op::Create,
                    w::Op::Update => pb::Op::Update,
                    w::Op::Delete => pb::Op::Delete,
                    w::Op::Adopt => pb::Op::Adopt,
                    w::Op::Replace => pb::Op::Replace,
                    w::Op::EndTick => pb::Op::EndTick,
                }
            }

            pub fn to_apply_request(r: &pb::ApplyRequest) -> w::ApplyRequest {
                w::ApplyRequest {
                    op: to_op(r.op),
                    type_: r.r#type.clone(),
                    name: r.name.clone(),
                    remote: r.remote.clone(),
                    config: to_doc(&r.config),
                    create_first: r.create_first,
                    assertions: r
                        .assertions
                        .iter()
                        .map(|a| w::Assertion {
                            path: a.path.clone(),
                            op: a.op.clone(),
                            value: to_doc(&a.value),
                            message: a.message.clone(),
                        })
                        .collect(),
                    spans: r
                        .spans
                        .iter()
                        .map(|s| w::Span {
                            addr: s.addr.clone(),
                            start_ms: s.start_ms,
                            end_ms: s.end_ms,
                        })
                        .collect(),
                    idempotency_key: r.idempotency_key.clone(),
                    keep: r.keep.clone(),
                }
            }

            pub fn from_apply_request(r: &w::ApplyRequest) -> R<pb::ApplyRequest> {
                Ok(pb::ApplyRequest {
                    op: from_op(r.op) as i32,
                    r#type: r.type_.clone(),
                    name: r.name.clone(),
                    remote: r.remote.clone(),
                    config: from_doc(&r.config)?,
                    create_first: r.create_first,
                    assertions: r
                        .assertions
                        .iter()
                        .map(|a| {
                            Ok(pb::Assertion {
                                path: a.path.clone(),
                                op: a.op.clone(),
                                value: from_doc(&a.value)?,
                                message: a.message.clone(),
                            })
                        })
                        .collect::<R<_>>()?,
                    spans: r
                        .spans
                        .iter()
                        .map(|s| pb::Span {
                            addr: s.addr.clone(),
                            start_ms: s.start_ms,
                            end_ms: s.end_ms,
                        })
                        .collect(),
                    idempotency_key: r.idempotency_key.clone(),
                    keep: r.keep.clone(),
                })
            }

            pub fn to_apply_response(r: &pb::ApplyResponse) -> w::ApplyResponse {
                w::ApplyResponse {
                    remote: r.remote.clone(),
                    attrs: to_doc(&r.attrs),
                    computed: to_doc(&r.computed),
                    elapsed_ms: r.elapsed_ms,
                    notes: r.notes.clone(),
                }
            }

            pub fn from_apply_response(r: &w::ApplyResponse) -> R<pb::ApplyResponse> {
                Ok(pb::ApplyResponse {
                    remote: r.remote.clone(),
                    attrs: from_doc(&r.attrs)?,
                    computed: from_doc(&r.computed)?,
                    elapsed_ms: r.elapsed_ms,
                    notes: r.notes.clone(),
                })
            }

            pub fn to_import_request(r: &pb::ImportRequest) -> w::ImportRequest {
                w::ImportRequest {
                    type_: r.r#type.clone(),
                    remote: r.remote.clone(),
                }
            }

            pub fn from_import_request(r: &w::ImportRequest) -> R<pb::ImportRequest> {
                Ok(pb::ImportRequest {
                    r#type: r.type_.clone(),
                    remote: r.remote.clone(),
                })
            }

            pub fn to_import_response(r: &pb::ImportResponse) -> w::ImportResponse {
                w::ImportResponse {
                    found: r.found,
                    type_: r.r#type.clone(),
                    name: r.name.clone(),
                    attrs: to_doc(&r.attrs),
                    computed: to_doc(&r.computed),
                }
            }

            pub fn from_import_response(r: &w::ImportResponse) -> R<pb::ImportResponse> {
                Ok(pb::ImportResponse {
                    found: r.found,
                    r#type: r.type_.clone(),
                    name: r.name.clone(),
                    attrs: from_doc(&r.attrs)?,
                    computed: from_doc(&r.computed)?,
                })
            }

            pub fn to_event(e: &pb::Event) -> w::Event {
                w::Event {
                    address: e.address.clone(),
                    status: e.status.clone(),
                    message: e.message.clone(),
                }
            }

            pub fn from_event(e: &w::Event) -> pb::Event {
                pb::Event {
                    address: e.address.clone(),
                    status: e.status.clone(),
                    message: e.message.clone(),
                }
            }

            fn to_held(h: &pb::Held) -> w::Held {
                w::Held {
                    provider: h.provider.clone(),
                    deployment: h.deployment.clone(),
                    type_: h.r#type.clone(),
                    remote: h.remote.clone(),
                    path: h.path.clone(),
                    digest: h.digest.clone(),
                }
            }

            fn from_held(h: &w::Held) -> pb::Held {
                pb::Held {
                    provider: h.provider.clone(),
                    deployment: h.deployment.clone(),
                    r#type: h.type_.clone(),
                    remote: h.remote.clone(),
                    path: h.path.clone(),
                    digest: h.digest.clone(),
                }
            }

            /// An absent held record is the empty one, which no provider
            /// holds.
            pub fn to_reveal_request(r: &pb::RevealRequest) -> w::RevealRequest {
                w::RevealRequest {
                    held: to_held(&r.held.clone().unwrap_or_default()),
                    lease: r.lease.clone(),
                }
            }

            pub fn from_reveal_request(r: &w::RevealRequest) -> R<pb::RevealRequest> {
                Ok(pb::RevealRequest {
                    held: Some(from_held(&r.held)),
                    lease: r.lease.clone(),
                })
            }

            pub fn to_reveal_response(r: &pb::RevealResponse) -> w::RevealResponse {
                w::RevealResponse {
                    value: r.value.clone(),
                }
            }

            pub fn from_reveal_response(r: &w::RevealResponse) -> R<pb::RevealResponse> {
                Ok(pb::RevealResponse {
                    value: r.value.clone(),
                })
            }
        }
    };
}
