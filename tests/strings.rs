//! String literals (docs/grammar.md "Strings and interpolation"): a string
//! spans lines (R-61), and `\` at a line end joins the line with the next.

mod common;
use common::Scratch;

/// `\` before a newline is no character: the lines join, the next line's
/// leading whitespace kept, with a hole on either line and inside
/// `str.dedent`. `fmt` leaves the string as written.
#[test]
fn a_backslash_at_a_line_end_joins_the_lines() {
    let s = Scratch::project("strings-join");
    let src = "input name: string = \"web\"\n\
               output one = \"a \\\n  b\"\n\
               output hole = \"${name}-\\\n${name}\"\n\
               output crlf = \"x\\\r\ny\"\n\
               output script = str.dedent(\n  \"\n  echo \\\n    ${name}\n\",\n)\n\
               use fake\n";
    s.write("main.df", src);
    let r = s
        .run(&["query", "attr(\"output\", \"\", k, v)", "main.df"])
        .success();
    for row in [
        "\"one\"     \"a   b\"",
        "\"hole\"    \"web-web\"",
        "\"crlf\"    \"xy\"",
        "\"script\"  \"echo     web\\n\"",
    ] {
        assert!(r.stdout.contains(row), "{row}\n{}", r.stdout);
    }
    s.run(&["fmt", "--check", "main.df"]).success();
    assert_eq!(s.read("main.df"), src);
}
