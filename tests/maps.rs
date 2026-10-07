//! `map(T)` (the After R-38/R-39 ticket's decisions): an object with
//! open keys, each value a `T`, for an input, a relation's column and a
//! schema attribute. `--set labels.team=x` and `set from` give one key,
//! `(k, v) in labels` takes each entry.

mod common;
mod tables_common;
use tables_common::scratch;

/// Whether a query's table has the row `cells`.
fn row(out: &str, cells: &[&str]) -> bool {
    out.lines()
        .any(|l| l.split_whitespace().eq(cells.iter().copied()))
}

/// An input typed `map(string)`: its default, a key given by `--set`
/// (read as the values' type, so `--set labels.n=1` is the string "1"),
/// each entry by `(k, v) in`; a value of another type is an error naming
/// the input.
#[test]
fn a_map_input_takes_a_key_by_set_and_iterates() {
    let s = scratch("map-input");
    s.write(
        "p.df",
        "\ninput labels: map(string) = { team: \"core\" }\n\
         input sizes: map(int) = {}\nuse fake\n\
         label(k, v) where (k, v) in labels\n\
         size(k, v) where (k, v) in sizes\n",
    );
    let r = s.run(&["query", "label(K, V)", "p.df"]).success();
    assert!(row(&r.stdout, &["\"team\"", "\"core\""]), "{}", r.stdout);
    let r = s
        .run(&[
            "query",
            "label(K, V)",
            "p.df",
            "--set",
            "labels.owner=ops",
            "--set",
            "labels.n=1",
        ])
        .success();
    for want in [
        ["\"team\"", "\"core\""],
        ["\"owner\"", "\"ops\""],
        ["\"n\"", "\"1\""],
    ] {
        assert!(row(&r.stdout, &want), "{want:?}: {}", r.stdout);
    }
    let r = s
        .run(&["query", "size(K, V)", "p.df", "--set", "sizes.a=3"])
        .success();
    assert!(row(&r.stdout, &["\"a\"", "3"]), "{}", r.stdout);
    let r = s
        .run(&["query", "size(K, V)", "p.df", "--set", "sizes.a=big"])
        .failure();
    assert!(
        r.stderr
            .contains("--set sizes.a=big: input sizes is map(int)"),
        "{}",
        r.stderr
    );
}

/// A schema attribute typed `map(string)` checks each value; a relation's
/// column typed `map(string)` checks each row's.
#[test]
fn a_map_attribute_and_column_check_their_values() {
    let s = scratch("map-attr");
    s.write(
        "p.df",
        "\nuse fake\nresource compute.vm a { labels = { team: \"core\", tier: 1 } }\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "compute.vm[\"a\"].labels is map(string): its key \"tier\" is string, not the int 1"
        ),
        "{}",
        r.stderr
    );
    s.write(
        "t.json",
        "[{\"n\": \"a\", \"tags\": {\"x\": \"1\"}}, {\"n\": \"b\", \"tags\": {\"x\": 2}}]",
    );
    s.write(
        "p.df",
        "\ninput t from json.decode(io.read(\"t.json\"))\ndecl t(n: string, tags: map(string))\nuse fake\n\
         resource compute.vm \"${n}\" { labels = tags } where t(n, tags)\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains("column tags: {x: 2} is not map(string)"),
        "{}",
        r.stderr
    );
    s.write("t.json", "[{\"n\": \"a\", \"tags\": {\"x\": \"1\"}}]");
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    assert!(
        r.stdout.contains("+ compute.vm[\"a\"]\n  labels.x = \"1\""),
        "{}",
        r.stdout
    );
}

/// A map field of an object input takes a key by its path; a relation
/// whose column is declared `map(int)` refuses a literal of another type.
#[test]
fn a_map_field_and_a_map_column() {
    let s = scratch("map-field");
    s.write(
        "p.df",
        "\ninput meta { owner: string = \"a\", labels: map(string) = {} }\nuse fake\n\
         l(k, v) where (k, v) in meta.labels\n",
    );
    let r = s
        .run(&["query", "l(K, V)", "p.df", "--set", "meta.labels.team=core"])
        .success();
    assert!(row(&r.stdout, &["\"team\"", "\"core\""]), "{}", r.stdout);
    s.write(
        "p.df",
        "\ndecl p(n: string, sizes: map(int))\nuse fake\np(\"a\", { x: \"big\" })\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "`p`'s column `sizes` is map(int): its key \"x\" is int, not the string \"big\""
        ),
        "{}",
        r.stderr
    );
}

/// `set from` gives a map input each key its document has, beside the
/// default's (the After R-38/R-39 ticket's item 3), read as the values'
/// type (a CSV cell as an int); a leaf beside the map is still a typo.
#[test]
fn set_from_gives_a_map_input_its_keys() {
    for (format, doc) in [
        ("yaml", "labels:\n  owner: ops\nsizes:\n  a: 3\n"),
        (
            "json",
            "{\"labels\": {\"owner\": \"ops\"}, \"sizes\": {\"a\": 3}}",
        ),
        ("toml", "[labels]\nowner = \"ops\"\n[sizes]\na = 3\n"),
        ("csv", "path,value\nlabels.owner,ops\nsizes.a,3\n"),
    ] {
        let s = scratch(&format!("map-set-from-{format}"));
        s.write(
            "p.df",
            &format!(
                "\ninput labels: map(string) = {{ team: \"core\" }}\n\
                 input sizes: map(int) = {{}}\nuse fake\n\
                 set from {format}.decode(io.read(\"c.{format}\"))\n\
                 label(k, v) where (k, v) in labels\n\
                 size(k, v) where (k, v) in sizes\n"
            ),
        );
        s.write(&format!("c.{format}"), doc);
        let r = s.run(&["query", "label(K, V)", "p.df"]).success();
        for want in [["\"team\"", "\"core\""], ["\"owner\"", "\"ops\""]] {
            assert!(row(&r.stdout, &want), "{format} {want:?}: {}", r.stdout);
        }
        let r = s.run(&["query", "size(K, V)", "p.df"]).success();
        assert!(row(&r.stdout, &["\"a\"", "3"]), "{format}: {}", r.stdout);
    }
    let s = scratch("map-set-from-typo");
    s.write(
        "p.df",
        "\ninput labels: map(string) = {}\nuse fake\nset from yaml.decode(io.read(\"c.yaml\"))\n",
    );
    s.write("c.yaml", "labels:\n  owner: ops\nlabelz:\n  x: y\n");
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("c.yaml:4: labelz.x is not an input (its inputs: labels)")
            && !r.stderr.contains("labels.owner"),
        "{}",
        r.stderr
    );
}
