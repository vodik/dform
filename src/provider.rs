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
