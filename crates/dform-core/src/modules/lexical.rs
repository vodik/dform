//! A component's body reads the items of the body around it bare
//! (R-186): its module's lets, inputs, resources and relations, and an
//! enclosing component's, as a Rust `fn` reads its module's. The scope
//! is lexical and by instance: the read carries the definition it names
//! (`Reach::Lexical`), and the copies being instanced make it the read of
//! that definition's instance the copy was taken from: the instance it is expanded inside (a copy made in
//! the module's own file), or the one a `use` binds (`a.volume` under
//! `use backups as a` reads `a`'s, never another `use`'s).

/// `__lexical_of(User, Name, Scope)`: the copy `Name` of the scope `User`
/// reads the items of the instance at `Scope` bare, what `why NAME` and
/// the hover answer from.
pub const LEXICAL_OF: &str = "__lexical_of";
