use crate::ast::Atom;
use crate::ir::{Adopt, Resource};
use crate::state::State;
use anyhow::Result;

#[derive(Debug, Clone)]
pub enum ActionKind {
    Create,
    Adopt,
    Update,
    Delete,
    Noop,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    /// The schema marks the path sensitive: print neither side.
    pub sensitive: bool,
}

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

/// A null or secret marker's key and label.
pub fn marker(v: &serde_json::Value) -> Option<(&'static str, &str)> {
    let m = v.as_object()?;
    if m.len() != 1 {
        return None;
    }
    for k in [NULL_KEY, SECRET_KEY] {
        if let Some(serde_json::Value::String(l)) = m.get(k) {
            return Some((k, l));
        }
    }
    None
}

/// Render one side of a change for plan output.
pub fn fmt_value(v: Option<&serde_json::Value>) -> String {
    let Some(v) = v else {
        return "<none>".to_string();
    };
    match marker(v) {
        Some((NULL_KEY, l)) => return format!("?{l}"),
        Some((_, l)) => return format!("(sensitive {l})"),
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
    pub addr: crate::ir::Address,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub actions: Vec<Action>,
}

pub trait Provider {
    fn id(&self) -> &str;

    /// Static facts about types/capabilities.
    fn catalog(&self) -> Result<Vec<Atom>>;

    /// Dynamic facts from the environment (inventory/discovery).
    fn discover(&self) -> Result<Vec<Atom>>;

    /// Optional migration hook (e.g. move old provider-owned state into core state).
    fn bootstrap_state(&self, _state: &mut State) -> Result<()> {
        Ok(())
    }

    fn plan(&self, desired: &[Resource], adopts: &[Adopt], state: &State) -> Result<Plan>;

    fn apply(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        state: &mut State,
        plan: &Plan,
    ) -> Result<()>;
}
