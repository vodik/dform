use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<Value>),
    Obj(BTreeMap<String, Value>),
    Ref { typ: String, name: String, attr: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Term {
    Val(Value),
    Var(String),
    Func { name: String, args: Vec<Term> },
}

impl Term {
    pub fn is_var(&self) -> bool {
        matches!(self, Term::Var(_))
    }
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}
