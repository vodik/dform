//! A kubeconfig's YAML as a tree: libyaml's loader (`unsafe-libyaml`,
//! serde_yaml's parser, `no_std`), its document's nodes copied out. An
//! alias is the node it names, as libyaml loads it.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem::MaybeUninit;

/// How deep a document may nest: a kubeconfig is four levels deep.
const DEPTH: usize = 64;

/// A YAML node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Yaml {
    /// A scalar's text, and whether it was written plain (unquoted): only
    /// a plain `true` is true. An empty document is the plain `""`.
    Scalar(String, bool),
    Seq(Vec<Yaml>),
    /// Its pairs in order, each key a scalar's text.
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    /// The first document of `text`; `None` when it is not YAML (or a key
    /// is not a scalar).
    pub fn parse(text: &[u8]) -> Option<Yaml> {
        let mut parser: Box<MaybeUninit<unsafe_libyaml::yaml_parser_t>> =
            Box::new(MaybeUninit::uninit());
        let mut doc: Box<MaybeUninit<unsafe_libyaml::yaml_document_t>> =
            Box::new(MaybeUninit::uninit());
        let (p, d) = (parser.as_mut_ptr(), doc.as_mut_ptr());
        // SAFETY: libyaml's protocol, as serde_yaml drives it: the parser
        // is initialized before use and deleted once, reading `text`,
        // which outlives it; a document is read only once loaded, and
        // deleted then; both live in boxes, so neither moves.
        unsafe {
            if unsafe_libyaml::yaml_parser_initialize(p).fail {
                return None;
            }
            unsafe_libyaml::yaml_parser_set_input_string(p, text.as_ptr(), text.len() as u64);
            let tree = match unsafe_libyaml::yaml_parser_load(p, d).fail {
                true => None,
                false => {
                    let root = unsafe_libyaml::yaml_document_get_root_node(d);
                    let tree = match root.is_null() {
                        true => Some(Yaml::Scalar(String::new(), true)),
                        false => node(d, root, 0),
                    };
                    unsafe_libyaml::yaml_document_delete(d);
                    tree
                }
            };
            unsafe_libyaml::yaml_parser_delete(p);
            tree
        }
    }

    /// The value of its key `k`, a mapping's.
    pub fn get(&self, k: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(pairs) => pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, k: &str) -> Option<&mut Yaml> {
        match self {
            Yaml::Map(pairs) => pairs.iter_mut().find(|(key, _)| key == k).map(|(_, v)| v),
            _ => None,
        }
    }

    /// A scalar's text.
    pub fn text(&self) -> Option<&str> {
        match self {
            Yaml::Scalar(s, _) => Some(s),
            _ => None,
        }
    }

    /// Its key `k`'s text.
    pub fn str(&self, k: &str) -> Option<&str> {
        self.get(k).and_then(Yaml::text)
    }

    /// Whether it is true, as YAML's core schema has it.
    pub fn is_true(&self) -> bool {
        matches!(self, Yaml::Scalar(s, true) if matches!(s.as_str(), "true" | "True" | "TRUE"))
    }

    /// A sequence's items; none of anything else.
    pub fn items(&self) -> &[Yaml] {
        match self {
            Yaml::Seq(items) => items,
            _ => &[],
        }
    }

    /// The item of its list `list` named `name` (`clusters`, `users`,
    /// `contexts`).
    pub fn named(&self, list: &str, name: &str) -> Option<&Yaml> {
        self.get(list)?
            .items()
            .iter()
            .find(|e| e.str("name") == Some(name))
    }
}

/// The node `n` of the document `d`, `depth` deep.
///
/// # Safety
///
/// `d` is a loaded document and `n` one of its nodes.
unsafe fn node(
    d: *mut unsafe_libyaml::yaml_document_t,
    n: *mut unsafe_libyaml::yaml_node_t,
    depth: usize,
) -> Option<Yaml> {
    if depth > DEPTH {
        return None;
    }
    // SAFETY: the caller's: `n` is a node of `d`, its data the variant its
    // type says, its stacks' items indices of `d`'s nodes.
    unsafe {
        match (*n).type_ {
            unsafe_libyaml::YAML_SCALAR_NODE => {
                let s = (*n).data.scalar;
                let bytes = core::slice::from_raw_parts(s.value, s.length as usize);
                let text = core::str::from_utf8(bytes).ok()?.to_string();
                Some(Yaml::Scalar(
                    text,
                    s.style == unsafe_libyaml::YAML_PLAIN_SCALAR_STYLE,
                ))
            }
            unsafe_libyaml::YAML_SEQUENCE_NODE => {
                let items = (*n).data.sequence.items;
                let mut out = Vec::new();
                let mut at = items.start;
                while at < items.top {
                    out.push(child(d, *at, depth)?);
                    at = at.add(1);
                }
                Some(Yaml::Seq(out))
            }
            unsafe_libyaml::YAML_MAPPING_NODE => {
                let pairs = (*n).data.mapping.pairs;
                let mut out = Vec::new();
                let mut at = pairs.start;
                while at < pairs.top {
                    let key = child(d, (*at).key, depth)?;
                    let value = child(d, (*at).value, depth)?;
                    out.push((key.text()?.to_string(), value));
                    at = at.add(1);
                }
                Some(Yaml::Map(out))
            }
            _ => None,
        }
    }
}

/// The node numbered `i` of the document `d`, under one `depth` deep.
///
/// # Safety
///
/// `d` is a loaded document.
unsafe fn child(d: *mut unsafe_libyaml::yaml_document_t, i: i32, depth: usize) -> Option<Yaml> {
    // SAFETY: the caller's; a number past its nodes is none.
    unsafe {
        let n = unsafe_libyaml::yaml_document_get_node(d, i);
        match n.is_null() {
            true => None,
            false => node(d, n, depth + 1),
        }
    }
}
