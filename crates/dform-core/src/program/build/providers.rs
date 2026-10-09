//! A provider's `use p [as n] { k = v .. } [where B]` as a `Provider`
//! item (R-211 step 5), from what the resolver lowered it to
//! ([`ProviderLowered`]): each setting's value the term's node after the
//! reads it hoisted (the helper statements its terms made its own), an
//! `expect_account` with the clause it is lowered under again, the clause
//! B's goals as gathered; which declaration of its name it is
//! (R-104).

use super::Builder;
use crate::ast::{Lit, Span, Stmt, Term};
use crate::program::node::{ClauseId, Item, ItemId, ItemKind, Setting};
use crate::program::scope::ScopeId;

/// A provider's `use` as the resolver lowered it.
pub struct ProviderLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    pub name: String,
    pub of: Option<String>,
    pub starts: bool,
    pub settings: Vec<SettingLowered<'a>>,
    pub clause: Option<ClauseId>,
    pub declared: Option<usize>,
    pub denies: Vec<Span>,
}

/// A setting as the resolver lowered it: its value, the reads it
/// hoisted, the helpers its terms made.
pub struct SettingLowered<'a> {
    pub kind: SettingKind,
    pub span: Span,
    pub value: &'a Term,
    pub reads: &'a [Lit],
    pub made: &'a [Stmt],
}

/// Which setting.
pub enum SettingKind {
    Source(String),
    Value(String),
    /// `expect_account`, under the clause gathered again.
    Account(Option<ClauseId>),
}

impl Builder<'_> {
    /// The `Provider` item of `p`.
    pub fn provider_item(&mut self, p: ProviderLowered) -> ItemId {
        let settings = p
            .settings
            .into_iter()
            .map(|s| {
                let value = self.at(s.span, |b| b.made(s.value, s.reads, s.made));
                let span = s.span;
                match s.kind {
                    SettingKind::Source(key) => Setting::Source { key, span, value },
                    SettingKind::Value(key) => Setting::Value { key, span, value },
                    SettingKind::Account(clause) => Setting::Account {
                        value,
                        clause,
                        span,
                    },
                }
            })
            .collect();
        let kind = ItemKind::Provider {
            name: p.name,
            of: p.of,
            starts: p.starts,
            settings,
            clause: p.clause,
            declared: p.declared,
            denies: p.denies,
        };
        self.program.items.insert(Item {
            span: p.span,
            scope: p.scope,
            kind,
        })
    }
}
