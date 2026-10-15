//! Literals in other spellings: an integer in another base by Rust's
//! prefixes, `0x1f`, `0o600`, `0b1010` (no leading-zero octal), whose value
//! is the number, printed in decimal; and a bare IPv6 address, `::1`,
//! `fe80::1/64`, which is its quoted spelling's value.

use crate::common;
use common::{error, facts};

#[test]
fn an_integer_in_another_base_is_its_number() {
    let src = "p(0x1f)\np(0o600)\np(0b1010)\np(600)\nq(m) where m = 0o644 + 0x10\n";
    let mut ps = facts(src, "p");
    ps.sort();
    assert_eq!(ps, ["p(10)", "p(31)", "p(384)", "p(600)"]);
    assert_eq!(facts(src, "q"), ["q(436)"]);
}

/// `0600` is no octal, as in Rust: an error naming both readings.
#[test]
fn a_leading_zero_is_an_error_naming_the_base() {
    let e = error("let mode = 0600\n");
    assert!(
        e.contains("`0600` has a leading zero, and a number's base is a prefix")
            && e.contains("help: `0o600` in octal, `600` in decimal"),
        "{e}"
    );
}

/// A bare IPv6 address is its text, as the quoted one is, wherever a
/// term goes: alone, in a list, in an object.
#[test]
fn a_bare_ipv6_address_is_its_quoted_spellings_value() {
    let bare = facts("p(::1)\np([fe80::1/64])\np({ net = 2001:db8::/32 })\n", "p");
    let quoted = facts(
        "p(\"::1\")\np([\"fe80::1/64\"])\np({ net = \"2001:db8::/32\" })\n",
        "p",
    );
    assert_eq!(bare, quoted);
    assert_eq!(bare.len(), 3, "{bare:?}");
}
