//! A component's body reads the items of the body around it bare
//! (R-186): its module's lets, inputs, resources and relations, and an
//! enclosing component's, as a Rust `fn` reads its module's. The scope
//! is lexical and by instance: the resolver marks such a read with the
//! definition it names ([`lexical_pred`], [`lexical_term`]), and
//! expansion makes it the read of that definition's instance the copy
//! was taken from: the instance it is expanded inside (a copy made in
//! the module's own file), or the one a `use` binds (`a.volume` under
//! `use backups as a` reads `a`'s, never another `use`'s).

use crate::ast::{Term, str_term};

/// The mark of a read of an enclosing definition's item: `__lexical(Body,
/// name)` as an address, `__lexical:Body:name` as a relation, Body the
/// definition's path.
pub const LEXICAL: &str = "__lexical";

/// `__lexical_of(User, Name, Scope)`: the copy `Name` of the scope `User`
/// reads the items of the instance at `Scope` bare, what `why NAME` and
/// the hover answer from.
pub const LEXICAL_OF: &str = "__lexical_of";

/// The relation a component's body reads an enclosing definition's
/// value or relation `name` by.
pub fn lexical_pred(body: &str, name: &str) -> String {
    format!("{LEXICAL}:{body}:{name}")
}

/// The address a component's body reads an enclosing definition's
/// resource `name` by.
pub fn lexical_term(body: &str, name: &str) -> Term {
    Term::Func {
        name: LEXICAL.into(),
        args: vec![str_term(body), str_term(name)],
    }
}

/// The name a [`lexical_pred`] reads, for a variable named after it.
pub fn lexical_name(pred: &str) -> Option<&str> {
    pred_parts(pred).map(|(_, n)| n)
}

/// The definition and the name a [`lexical_pred`] reads.
pub fn lexical_parts(pred: &str) -> Option<(&str, &str)> {
    pred_parts(pred)
}

/// The definition and the name a [`lexical_pred`] reads.
fn pred_parts(pred: &str) -> Option<(&str, &str)> {
    pred.strip_prefix(LEXICAL)?
        .strip_prefix(':')?
        .rsplit_once(':')
}
