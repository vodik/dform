//! Statements as text: every one the provider sends is the simple query
//! protocol's, so each name and value in it is quoted here, as
//! `quote_ident` and `quote_literal` do in the server.

/// `name` as an identifier: `"name"`, a `"` doubled.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `value` as a string literal: `'value'`, a `'` doubled; one holding a
/// backslash is an escape string (`E'..'`) with it doubled too, so the
/// text is the same whatever `standard_conforming_strings` says.
pub fn literal(value: &str) -> String {
    let quoted = value.replace('\'', "''");
    match value.contains('\\') {
        true => format!("E'{}'", quoted.replace('\\', "\\\\")),
        false => format!("'{quoted}'"),
    }
}

/// A name no statement may carry: empty, or with a NUL (the protocol's
/// terminator), or longer than the server keeps (NAMEDATALEN - 1).
pub fn check_name(what: &str, name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("{what} is empty");
    }
    if name.contains('\0') {
        anyhow::bail!("{what} holds a NUL character");
    }
    if name.len() > 63 {
        anyhow::bail!(
            "{what} is {} bytes long: Postgres keeps 63 and would truncate it",
            name.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_values_are_quoted() {
        assert_eq!(ident("synapse"), "\"synapse\"");
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(literal("a\\b'"), "E'a\\\\b'''");
        assert!(check_name("name", &"x".repeat(64)).is_err());
        assert!(check_name("name", "a\0b").is_err());
    }
}
