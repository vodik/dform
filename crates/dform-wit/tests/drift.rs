//! The WIT package (`wit/dform-provider.wit`) and the proto
//! (`proto/dform/v1/provider.proto`) state one contract: the same calls,
//! the same messages, the same fields with the same shapes. This walks
//! both, so neither changes without the other (DESIGN.org R-13).
//!
//! The mapping, as the WIT's header records it: names kebab-cased; a
//! message is a record of the same name, except `Value`, the variant
//! `value`, whose `List`/`Obj` children are `node` indices; a `Value`
//! field is a `tree`; `optional` is `option`, `repeated` is `list`, a map
//! is a list of pairs; an enum loses its `*_UNSPECIFIED` case; `bytes`
//! is `list<u8>`; every call is an `async func` answering
//! `result<_, call-error>`, a server stream of rows a list; a server
//! stream of a message that is a oneof of an event and a `result` (Apply's
//! `ApplyEvent`) is a `stream` of the event and a `future` of the result,
//! and that message is the proto's alone.

use std::collections::BTreeSet;
use std::path::Path;

use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type as Pt};
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};
use wit_parser::{FunctionKind, InterfaceId, Resolve, Type, TypeDefKind, TypeId};

/// WIT types with no proto message: the encoding's own.
const WIT_ONLY: &[&str] = &["tree", "node", "call-error"];

/// Proto messages with no WIT type: a stream's messages that the WIT
/// carries as a stream and a future.
const PROTO_ONLY: &[&str] = &["ApplyEvent"];

struct Contract {
    proto: FileDescriptorProto,
    resolve: Resolve,
    types: InterfaceId,
    provider: InterfaceId,
}

fn contract() -> Contract {
    let set =
        FileDescriptorSet::decode(&include_bytes!(concat!(env!("OUT_DIR"), "/provider.fds"))[..])
            .unwrap();
    let proto = set
        .file
        .into_iter()
        .find(|f| f.name() == "dform/v1/provider.proto")
        .unwrap();
    let mut resolve = Resolve::new();
    let wit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit");
    let (pkg, _) = resolve.push_dir(&wit).unwrap();
    let iface = |name: &str| resolve.packages[pkg].interfaces[name];
    let (types, provider) = (iface("types"), iface("provider"));
    Contract {
        proto,
        resolve,
        types,
        provider,
    }
}

/// `HandshakeRequest` -> `handshake-request`, `protocol_version` ->
/// `protocol-version`, `END_TICK` -> `end-tick`.
fn kebab(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c == '_' {
            out.push('-');
        } else if c.is_ascii_uppercase() && s.contains(|c: char| c.is_ascii_lowercase()) {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// `.dform.v1.HandshakeRequest` -> `HandshakeRequest`.
fn short(type_name: &str) -> &str {
    type_name.rsplit('.').next().unwrap()
}

impl Contract {
    fn wit_type(&self, name: &str) -> TypeId {
        *self.resolve.interfaces[self.types]
            .types
            .get(name)
            .unwrap_or_else(|| panic!("the WIT has no type `{name}`"))
    }

    fn kind(&self, ty: &Type) -> Option<&TypeDefKind> {
        match ty {
            Type::Id(id) => Some(&self.resolve.types[*id].kind),
            _ => None,
        }
    }

    /// A WIT type's name, if it is a named one.
    fn named(&self, ty: &Type) -> Option<&str> {
        match ty {
            Type::Id(id) => self.resolve.types[*id].name.as_deref(),
            _ => None,
        }
    }

    /// The WIT type a proto field's element (one item of a repeated
    /// field) must be; `in_value` for `List` and `Obj`, whose values are
    /// nodes of the tree they are in.
    fn expect_scalar(&self, f: &FieldDescriptorProto, ty: &Type, in_value: bool, at: &str) {
        let want = match f.r#type() {
            Pt::String => Type::String,
            Pt::Bool => Type::Bool,
            Pt::Uint32 => Type::U32,
            Pt::Int64 => Type::S64,
            Pt::Uint64 => Type::U64,
            Pt::Double => Type::F64,
            Pt::Bytes => {
                let Some(TypeDefKind::List(Type::U8)) = self.kind(ty) else {
                    panic!("{at}: bytes in the proto, not list<u8> in the WIT");
                };
                return;
            }
            Pt::Message | Pt::Enum => {
                let want = match short(f.type_name()) {
                    "Value" if in_value => "node".to_string(),
                    "Value" => "tree".to_string(),
                    other => kebab(other),
                };
                assert_eq!(self.named(ty), Some(&*want), "{at}: want `{want}`");
                return;
            }
            other => panic!("{at}: the drift test does not map proto type {other:?}"),
        };
        assert_eq!(*ty, want, "{at}");
    }

    /// A proto field against a WIT record field or variant case type.
    fn expect_field(&self, msg: &DescriptorProto, f: &FieldDescriptorProto, ty: &Type, at: &str) {
        let in_value = matches!(msg.name(), "List" | "Obj");
        if f.label() == Label::Repeated {
            let Some(TypeDefKind::List(item)) = self.kind(ty) else {
                panic!("{at}: repeated in the proto, not a list in the WIT");
            };
            let entry = msg
                .nested_type
                .iter()
                .find(|n| n.options.as_ref().is_some_and(|o| o.map_entry()))
                .filter(|n| short(f.type_name()) == n.name());
            match entry {
                // A map: a list of (key, value) pairs.
                Some(entry) => {
                    let Some(TypeDefKind::Tuple(t)) = self.kind(item) else {
                        panic!("{at}: a map in the proto, not a list of pairs in the WIT");
                    };
                    assert_eq!(t.types.len(), 2, "{at}");
                    self.expect_scalar(&entry.field[0], &t.types[0], in_value, at);
                    self.expect_scalar(&entry.field[1], &t.types[1], in_value, at);
                }
                None => self.expect_scalar(f, item, in_value, at),
            }
        } else if f.proto3_optional() {
            let Some(TypeDefKind::Option(inner)) = self.kind(ty) else {
                panic!("{at}: optional in the proto, not an option in the WIT");
            };
            self.expect_scalar(f, inner, in_value, at);
        } else {
            assert!(
                !matches!(self.kind(ty), Some(TypeDefKind::Option(_))),
                "{at}: an option in the WIT, not optional in the proto"
            );
            self.expect_scalar(f, ty, in_value, at);
        }
    }
}

/// The `result<OK, call-error>` a call answers: its ok type.
fn ok_of<'a>(c: &'a Contract, ty: &'a Type, at: &str) -> &'a Type {
    let Some(TypeDefKind::Result(r)) = c.kind(ty) else {
        panic!("{at}: not a result");
    };
    assert_eq!(c.named(r.err.as_ref().unwrap()), Some("call-error"), "{at}");
    r.ok.as_ref().unwrap()
}

#[test]
fn the_calls_match() {
    let c = contract();
    let service = &c.proto.service[0];
    assert_eq!(service.name(), "Provider");
    let funcs = &c.resolve.interfaces[c.provider].functions;
    let rpcs: Vec<String> = service.method.iter().map(|m| kebab(m.name())).collect();
    let wit: Vec<&String> = funcs.keys().collect();
    assert_eq!(wit, rpcs.iter().collect::<Vec<_>>(), "the calls, in order");
    for m in &service.method {
        let at = format!("call {}", m.name());
        let f = &funcs[&kebab(m.name())];
        assert_eq!(f.kind, FunctionKind::AsyncFreestanding, "{at}: not async");
        assert_eq!(f.params.len(), 1, "{at}");
        assert_eq!(
            c.named(&f.params[0].ty),
            Some(&*kebab(short(m.input_type()))),
            "{at}: its request"
        );
        let result = f.result.as_ref().expect("an answer");
        let output = c
            .proto
            .message_type
            .iter()
            .find(|d| d.name() == short(m.output_type()))
            .unwrap();
        let ok = match (m.server_streaming(), c.kind(result)) {
            // An event stream: the oneof's other arm is the result, the
            // WIT's future.
            (true, Some(TypeDefKind::Tuple(t))) => {
                assert!(PROTO_ONLY.contains(&output.name()), "{at}");
                assert_eq!(t.types.len(), 2, "{at}: a stream and a future");
                assert_eq!(output.oneof_decl.len(), 1, "{at}: one oneof");
                let arm = |name: &str| {
                    let f = output.field.iter().find(|f| f.name() == name);
                    let f = f.unwrap_or_else(|| panic!("{at}: no `{name}` arm"));
                    assert!(
                        f.oneof_index.is_some(),
                        "{at}: `{name}` is not of the oneof"
                    );
                    kebab(short(f.type_name()))
                };
                assert_eq!(output.field.len(), 2, "{at}: an event and a result");
                let Some(TypeDefKind::Stream(Some(event))) = c.kind(&t.types[0]) else {
                    panic!("{at}: not a stream first");
                };
                assert_eq!(c.named(event), Some(&*arm("event")), "{at}: its events");
                let Some(TypeDefKind::Future(Some(done))) = c.kind(&t.types[1]) else {
                    panic!("{at}: not a future second");
                };
                assert_eq!(c.named(ok_of(&c, done, &at)), Some(&*arm("result")), "{at}");
                continue;
            }
            (true, _) => {
                let Some(TypeDefKind::List(item)) = c.kind(ok_of(&c, result, &at)) else {
                    panic!("{at}: a stream in the proto, not a list in the WIT");
                };
                item
            }
            (false, _) => ok_of(&c, result, &at),
        };
        assert_eq!(
            c.named(ok),
            Some(&*kebab(short(m.output_type()))),
            "{at}: its response"
        );
    }
}

#[test]
fn the_messages_match() {
    let c = contract();
    let mut proto_names = BTreeSet::new();
    for msg in &c.proto.message_type {
        if PROTO_ONLY.contains(&msg.name()) {
            continue;
        }
        let name = kebab(msg.name());
        proto_names.insert(name.clone());
        let ty = c.wit_type(&name);
        match &c.resolve.types[ty].kind {
            // Value: its oneof's arms are the variant's cases.
            TypeDefKind::Variant(v) => {
                assert_eq!(msg.name(), "Value");
                assert_eq!(msg.oneof_decl.len(), 1);
                let arms: Vec<String> = msg.field.iter().map(|f| kebab(f.name())).collect();
                let cases: Vec<&String> = v.cases.iter().map(|c| &c.name).collect();
                assert_eq!(cases, arms.iter().collect::<Vec<_>>(), "Value's arms");
                for (f, case) in msg.field.iter().zip(&v.cases) {
                    let at = format!("Value.{}", f.name());
                    assert!(f.oneof_index.is_some(), "{at}");
                    let ty = case.ty.as_ref().unwrap();
                    c.expect_field(msg, f, ty, &at);
                }
            }
            TypeDefKind::Record(r) => {
                // Only `optional`'s synthetic oneofs: a real one is a variant.
                assert!(
                    msg.field
                        .iter()
                        .all(|f| f.oneof_index.is_none() || f.proto3_optional()),
                    "{}: a oneof, not a variant in the WIT",
                    msg.name()
                );
                let fields: Vec<String> = msg.field.iter().map(|f| kebab(f.name())).collect();
                let wit: Vec<&String> = r.fields.iter().map(|f| &f.name).collect();
                assert_eq!(
                    wit,
                    fields.iter().collect::<Vec<_>>(),
                    "{}'s fields",
                    msg.name()
                );
                for (f, wf) in msg.field.iter().zip(&r.fields) {
                    c.expect_field(msg, f, &wf.ty, &format!("{}.{}", msg.name(), f.name()));
                }
            }
            other => panic!("{}: a {other:?} in the WIT", msg.name()),
        }
    }
    for e in &c.proto.enum_type {
        let name = kebab(e.name());
        proto_names.insert(name.clone());
        let TypeDefKind::Enum(w) = &c.resolve.types[c.wit_type(&name)].kind else {
            panic!("{}: not an enum in the WIT", e.name());
        };
        let values: Vec<String> = e
            .value
            .iter()
            .filter(|v| !v.name().ends_with("_UNSPECIFIED"))
            .map(|v| kebab(v.name()))
            .collect();
        let cases: Vec<&String> = w.cases.iter().map(|c| &c.name).collect();
        assert_eq!(
            cases,
            values.iter().collect::<Vec<_>>(),
            "{}'s values",
            e.name()
        );
    }
    // And the WIT has nothing the proto lacks but the encoding's own.
    for name in c.resolve.interfaces[c.types].types.keys() {
        assert!(
            proto_names.contains(name) || WIT_ONLY.contains(&&**name),
            "the WIT's `{name}` is not in the proto"
        );
    }
}

/// call-error's cases are dform-core's `CallError` variants.
#[test]
fn call_error_is_core_call_error() {
    let c = contract();
    let TypeDefKind::Variant(v) = &c.resolve.types[c.wit_type("call-error")].kind else {
        panic!("call-error is not a variant");
    };
    let cases: Vec<&str> = v.cases.iter().map(|c| c.name.as_str()).collect();
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../dform-core/src/plugin/backend.rs"),
    )
    .unwrap();
    let body = src
        .split("pub enum CallError {")
        .nth(1)
        .and_then(|s| s.split('}').next())
        .expect("backend.rs has `pub enum CallError`");
    let variants: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.is_empty())
        .map(|l| kebab(l.split('(').next().unwrap()))
        .collect();
    assert_eq!(cases, variants);
}

/// Proto services of the host with no WIT interface a provider exports:
/// what dform serves (`Host`), what a provider declares (`Manifest`), and
/// `Io` by its name before R-155 (`Files`), kept so a provider built then
/// still serves its schemes.
const HOST_PROTO_ONLY: &[&str] = &["Host", "Manifest", "Files"];

/// `io` -> `Io`, `read-file` -> `ReadFile`.
fn pascal(s: &str) -> String {
    s.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

/// The host's contract (wit/host, proto/dform/host/v1/host.proto): each
/// interface a provider exports to the host (`world scheme-provider`,
/// beyond `hosted-provider`) is a service of the host proto by its name,
/// its functions the service's calls (R-155: `io`, never `files`), and
/// the proto has no other service but those [`HOST_PROTO_ONLY`] names.
#[test]
fn the_hosts_exports_are_its_services() {
    let set = FileDescriptorSet::decode(&include_bytes!(concat!(env!("OUT_DIR"), "/host.fds"))[..])
        .unwrap();
    let proto = set
        .file
        .into_iter()
        .find(|f| f.name() == "dform/host/v1/host.proto")
        .unwrap();
    let mut resolve = Resolve::new();
    let wit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit/host");
    let (pkg, _) = resolve.push_dir(&wit).unwrap();
    let world = resolve.packages[pkg].worlds["scheme-provider"];
    let mut exported = BTreeSet::new();
    for (key, item) in &resolve.worlds[world].exports {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let iface = &resolve.interfaces[*id];
        if iface.package != Some(pkg) {
            continue;
        }
        let name = iface
            .name
            .clone()
            .unwrap_or_else(|| resolve.name_world_key(key));
        let service = pascal(&name);
        let s = proto
            .service
            .iter()
            .find(|s| s.name() == service)
            .unwrap_or_else(|| panic!("the WIT's `{name}` has no proto service `{service}`"));
        let wit_calls: BTreeSet<String> = iface.functions.keys().map(|f| pascal(f)).collect();
        let proto_calls: BTreeSet<String> = s.method.iter().map(|m| m.name().to_string()).collect();
        assert_eq!(wit_calls, proto_calls, "{name}'s calls");
        exported.insert(service);
    }
    assert!(exported.contains("Io"), "{exported:?}");
    for s in &proto.service {
        assert!(
            exported.contains(s.name()) || HOST_PROTO_ONLY.contains(&s.name()),
            "the proto service {} is no WIT interface's",
            s.name()
        );
    }
}
