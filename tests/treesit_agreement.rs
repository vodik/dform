//! The editor grammar (`tree-sitter-dform/`) and the compiler's parser
//! (`crates/dform-core/src/syntax/parser.rs`) read the corpus the same way: every file the
//! compiler reads parses without an ERROR or MISSING node, both trees have
//! the same statements, blocks and literals at the same byte ranges, and a
//! file has a tree-sitter error exactly when it has a syntax error. The
//! highlight query's reference capture fires on field values, not reads.

mod common;
use common::repo;
use dform::syntax::SyntaxKind::{self, *};
use dform::syntax::parser::parse;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

fn df_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if dir != repo() {
                df_files(&p, out);
            }
        } else if p.extension().is_some_and(|e| e == "df") {
            out.push(p);
        }
    }
}

/// Every `.df` the repository ships and `tests/syntax/ok` (as tests/fmt.rs).
fn corpus() -> Vec<PathBuf> {
    let mut out = Vec::new();
    df_files(repo(), &mut out);
    for d in [
        "modules",
        "policies",
        "examples",
        "providers",
        "tests/syntax/ok",
    ] {
        df_files(&repo().join(d), &mut out);
    }
    out
}

fn errs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    df_files(&repo().join("tests/syntax/err"), &mut out);
    out
}

fn name(f: &Path) -> String {
    f.strip_prefix(repo()).unwrap().display().to_string()
}

fn ts_parse(src: &str) -> Tree {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_dform::LANGUAGE.into())
        .expect("the dform grammar loads");
    p.parse(src, None).expect("tree-sitter returns a tree")
}

/// Each ERROR and MISSING node, as `line:col kind`.
fn ts_errors(tree: &Tree) -> Vec<String> {
    fn walk(n: Node, out: &mut Vec<String>) {
        if n.is_error() || n.is_missing() {
            let p = n.start_position();
            let what = if n.is_missing() {
                format!("MISSING {}", n.kind())
            } else {
                "ERROR".to_string()
            };
            out.push(format!("{}:{} {what}", p.row + 1, p.column + 1));
            return;
        }
        if n.has_error() {
            let mut c = n.walk();
            for ch in n.children(&mut c) {
                walk(ch, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(tree.root_node(), &mut out);
    out
}

/// The nodes both trees have, tree-sitter's name for each compiler kind.
/// Terms that are chains, indexes and types are left out: the two trees
/// cut them differently (tree-sitter nests `a.b[c]`, rowan lists it).
const SAME: &[(SyntaxKind, &[&str])] = &[
    (EDITION, &["edition"]),
    (IMPORT, &["import"]),
    (PROVIDER, &["provider"]),
    (STACK, &["stack"]),
    (INPUT, &["input"]),
    (INPUT_RELATION, &["input_relation"]),
    (OUTPUT_DECL, &["output"]),
    (EXPORT, &["export"]),
    (CONTRIBUTES, &["contributes"]),
    (EXTERN, &["extern"]),
    (TYPE_DECL, &["type_declaration"]),
    (ATTR_DECL, &["attribute_declaration"]),
    (DECL, &["decl"]),
    (MODULE, &["module"]),
    (INSTANCE, &["instance"]),
    (POLICY, &["policy"]),
    (APPLY, &["apply"]),
    (SCENARIO, &["scenario"]),
    (WHEN, &["when"]),
    (FOR_STMT, &["for"]),
    (LET, &["let"]),
    (WITH, &["with"]),
    (RESOURCE, &["resource"]),
    (SETTINGS, &["settings"]),
    (BLOCK, &["block"]),
    (CLAUSE, &["clause"]),
    (ASSIGN, &["field"]),
    (BLOCK_PATH, &["block_path"]),
    (STMT_BLOCK, &["statement_block"]),
    (CHECK, &["check"]),
    (CONTRIBUTION, &["contribution"]),
    (VALUE_RULE, &["value_rule"]),
    (RULE, &["rule"]),
    (FACT, &["fact"]),
    (WHERE_CLAUSE, &["where_clause"]),
    (BODY, &["body", "body_block"]),
    (LIT_ATOM, &["atom_literal"]),
    (LIT_NOT, &["not_literal"]),
    (LIT_NOT_BLOCK, &["not_block"]),
    (LIT_CMP, &["comparison"]),
    (LIT_IN, &["in_literal"]),
    (LIT_NOT_IN, &["not_in_literal"]),
    (LIT_TRUTH, &["truth_literal"]),
    (LIT_EXISTS, &["exists_literal"]),
    (LIT_HAS, &["has_literal"]),
    (LIT_SOME, &["some_literal"]),
    (CALL, &["call"]),
    (RECORD_ATOM, &["record"]),
    (RECORD_FIELD, &["record_field"]),
    (LIST, &["list"]),
    (OBJECT, &["object"]),
    (COMPREHENSION, &["comprehension"]),
    (PAREN, &["parenthesized"]),
    (BIN_EXPR, &["binary_expression"]),
    (UNARY_EXPR, &["unary_expression"]),
    (PATH_LIT, &["keypath"]),
];

type Shape = Vec<(String, usize, usize)>;

fn rowan_shape(src: &str) -> Shape {
    let mut out: Shape = parse(src)
        .syntax()
        .descendants()
        .filter_map(|n| {
            let (_, names) = SAME.iter().find(|(k, _)| *k == n.kind())?;
            let r = n.text_range();
            Some((names[0].to_string(), r.start().into(), r.end().into()))
        })
        .collect();
    out.sort();
    out
}

fn ts_shape(tree: &Tree) -> Shape {
    fn walk(n: Node, out: &mut Shape) {
        if let Some((_, names)) = SAME.iter().find(|(_, names)| names.contains(&n.kind())) {
            out.push((names[0].to_string(), n.start_byte(), n.end_byte()));
        }
        // The compiler parses a string's holes while lowering it.
        if n.kind() == "string" {
            return;
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            walk(ch, out);
        }
    }
    let mut out = Vec::new();
    walk(tree.root_node(), &mut out);
    out.sort();
    out
}

#[test]
fn every_corpus_file_parses_without_error_nodes() {
    let files = corpus();
    assert!(files.len() > 30, "the corpus is {} files", files.len());
    let mut bad = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        let errors = ts_errors(&ts_parse(&src));
        if !errors.is_empty() {
            bad.push(format!("{}: {}", name(&f), errors.join(", ")));
        }
    }
    assert!(bad.is_empty(), "tree-sitter errors:\n{}", bad.join("\n"));
}

#[test]
fn both_parsers_build_the_same_statements_and_literals() {
    let mut bad = Vec::new();
    for f in corpus() {
        let src = std::fs::read_to_string(&f).unwrap();
        let (want, got) = (rowan_shape(&src), ts_shape(&ts_parse(&src)));
        if want == got {
            continue;
        }
        let text = |(k, s, e): &(String, usize, usize)| {
            let at = src[..*s].lines().count().max(1);
            format!("{k} at line {at}: {:?}", &src[*s..*e])
        };
        let only_rowan: Vec<_> = want.iter().filter(|n| !got.contains(n)).map(text).collect();
        let only_ts: Vec<_> = got.iter().filter(|n| !want.contains(n)).map(text).collect();
        bad.push(format!(
            "{}:\n  only the compiler's: {only_rowan:#?}\n  only tree-sitter's: {only_ts:#?}",
            name(&f)
        ));
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The files whose errors are in a string's text or holes: the compiler
/// finds those while lowering the string (crates/dform-core/src/syntax/resolve.rs), the
/// editor grammar while parsing it.
const STRING_ERRORS: &[&str] = &["bad_string.df", "interpolation.df"];

/// A file of `tests/syntax/err` has a tree-sitter error exactly when it has
/// a syntax error. The rest of that directory is errors of names, modules,
/// externs and the edition, which the resolver finds on a clean tree.
#[test]
fn syntax_errors_agree() {
    let mut bad = Vec::new();
    let mut syntax = 0;
    for f in errs() {
        let file = f.file_name().unwrap().to_str().unwrap();
        let src = std::fs::read_to_string(&f).unwrap();
        let compiler = !parse(&src).errors.is_empty();
        let in_string = STRING_ERRORS.contains(&file);
        assert!(
            !(compiler && in_string),
            "{file} is in STRING_ERRORS but the parser rejects it: take it out"
        );
        let want = compiler || in_string;
        let got = ts_errors(&ts_parse(&src));
        syntax += usize::from(want);
        if want == got.is_empty() {
            bad.push(format!(
                "{}: the compiler's parser {}, tree-sitter {}",
                name(&f),
                if compiler {
                    "rejects it"
                } else if in_string {
                    "lowers its strings with an error"
                } else {
                    "accepts it"
                },
                if got.is_empty() {
                    "finds no error".to_string()
                } else {
                    format!("finds {}", got.join(", "))
                }
            ));
        }
    }
    assert!(syntax >= 8, "only {syntax} syntax-error files");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Each node the highlight query captures under `name`.
fn captures<'t>(tree: &'t Tree, src: &str, name: &str) -> Vec<Node<'t>> {
    let lang = tree_sitter_dform::LANGUAGE.into();
    let q = Query::new(&lang, tree_sitter_dform::HIGHLIGHTS_QUERY).expect("highlights.scm");
    let idx = q.capture_index_for_name(name).expect("the capture exists");
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut it = cursor.captures(&q, tree.root_node(), src.as_bytes());
    while let Some((m, i)) = it.next() {
        let c = m.captures[*i];
        if c.index == idx && !out.contains(&c.node) {
            out.push(c.node);
        }
    }
    out
}

fn inside(n: Node, kinds: &[&str]) -> bool {
    std::iter::successors(n.parent(), |p| p.parent()).any(|p| kinds.contains(&p.kind()))
}

/// Proposal G, G-6: a dot in a field's value is a reference, in a rule body
/// or a clause a read. dform.df has both: `requester_vpc_id = a.id` in a
/// block, `a = network[ia].vpc` in a rule body.
#[test]
fn the_reference_capture_is_on_field_values_and_not_on_reads() {
    let src = std::fs::read_to_string(repo().join("dform.df")).unwrap();
    let tree = ts_parse(&src);
    let refs = captures(&tree, &src, "variable.reference");
    let text = |n: &Node| src[n.byte_range()].to_string();
    let texts: Vec<String> = refs.iter().map(text).collect();
    for want in ["a.id", "b.id"] {
        assert!(
            texts.iter().any(|t| t == want),
            "{want} not captured: {texts:?}"
        );
    }
    for n in &refs {
        assert!(
            !inside(
                *n,
                &[
                    "body",
                    "body_block",
                    "clause",
                    "interpolation",
                    "index_expression"
                ]
            ),
            "{} at line {} is a read, captured as a reference",
            text(n),
            n.start_position().row + 1
        );
    }
    // Every dotted field value is captured; every dot in a body is not.
    let mut c = tree.walk();
    let mut stack = vec![tree.root_node()];
    let (mut values, mut reads) = (0, 0);
    while let Some(n) = stack.pop() {
        stack.extend(n.named_children(&mut c));
        if n.kind() != "member_expression" {
            continue;
        }
        let parent = n.parent().unwrap();
        if parent.kind() == "field" && parent.child_by_field_name("value") == Some(n) {
            values += 1;
            assert!(
                refs.contains(&n),
                "field value {} is not captured",
                text(&n)
            );
        }
        if inside(n, &["body", "body_block"]) {
            reads += 1;
            assert!(!refs.contains(&n), "body read {} is captured", text(&n));
        }
    }
    assert!(
        values >= 4 && reads >= 4,
        "{values} field values, {reads} body reads"
    );
}

/// Lines, glue and keywords-as-names, where the two parsers could part.
/// Each snippet: both accept it with the same tree, or both reject it.
#[test]
fn edge_cases_agree() {
    let snippets = [
        // A newline ends a statement; a trailing operator, `,` or `if`
        // continues it; a leading one does not.
        "x = 1 +\n  2\n",
        "x = 1\n  + 2\n",
        "p(a) if q(a),\n  r(a)\n",
        "p(a) if q(a)\n  , r(a)\n",
        "p(a) if q(a)\n  # c\n\n  , r(a)\n",
        "p(a)\n  @override\n",
        "p(a) if q(a) # c\nr(a)\n",
        "p(a)\n  if q(a)\n",
        "p(a) if\n  q(a)\n",
        "p(a) q(b)\n",
        "p(a).\n",
        "p(a) :- q(a)\n",
        "p(a) if { q(a)\n  r(a) }\n",
        "p(a) if {\n  q(a), r(a)\n\n  s(a)\n}\n",
        "p(a) if { q(a) r(a) }\n",
        "p(\n  a,\n  b,\n)\n",
        "k = [\n  1,\n  2\n]\n",
        "k = { a: 1,\n  b }\n",
        "k = [x | q(x),\n  r(x)]\n",
        "k = [x | q(x)\n  r(x)]\n",
        "k = [x ordered by x | q(x)]\n",
        // Blocks: clauses first, entries by newline or comma.
        "resource net.vpc a { for q(x), r(x)\n  cidr = x, name = \"a\" }\n",
        "resource net.vpc a { cidr = \"x\" name = \"a\" }\n",
        "resource net.vpc a\n{ cidr = \"x\" }\n",
        "resource net.vpc \"a-{x}\" @default {\n  for q(x)\n\n  # c\n  tags.team = \"x\"\n  list[0].name = x\n  audit.sinks += [\"s3\"] @override\n}\n",
        "provider fake {}\n",
        "settings e @default {\n  for env(e)\n}\n",
        "settings.prod.audit.sinks = [\"s3\"]\n",
        "x.tags = {} if x in resource\n",
        // Glue: calls, records, indexes, instance paths, keypaths.
        "p (a)\n",
        "p(x) if y = a/b, z = x/2, w = x / 2, v = m.i/n.p\n",
        "p(x) if y = t[x].p, z = f[0].x, q(.tags.\"a.b\"[0], .id)\n",
        "names(n) if project{ network_name: n }\n",
        "names(n) if project { network_name: n }\n",
        "p(x) if x.\"a-b\".type == \"c\"\n",
        // Keywords as names.
        "input(\"env\", v) if q(v)\n",
        "deny(\"m\", \"t\", { a: b }) if q(b)\n",
        "output(\"k\", v) if q(v)\n",
        "deny \"m\" { a } if q(a)\n",
        "deny \"m\"\n{ a }\n",
        "input relation: string = \"x\"\n",
        "input relation seen/2 from file(\"seen.facts\")\n",
        "p(x) if x = { type: \"a\", input: 1 }\n",
        "p(x) if not { q(x) }, not r(x), x not in xs, not x in net.vpc\n",
        "p(x) if some i, v in xs, has x.a, exists x, 1 <= x <= 3\n",
        "when env == \"prod\" {\n  q(1)\n}\n",
        "for z in zones {\n  q(z)\n}\n",
        "module m {\n  input on: bool = true where on\n  output v: net.vpc\n  export q/1\n  contributes _.tags\n}\n",
        "extern file.json(+path, -value: string) persist\n",
        "decl p/2 mixed\ndecl q(a: int, b: list(string))\ndecl type k8s.x open\n",
        "type db.pg {\n  backup_days: int required where 1 <= backup_days\n  net: { cidr: inet, id: string computed id }\n}\n",
        "import \"a.df\" as a\n",
        "apply p\nlet cfg = settings[env]\nscenario s {\n  with env = \"prod\"\n}\n",
        // Strings.
        "p(\"a {x} {{b}} \\\"c\\\" \\u{41}\") if q(x)\n",
        "p(\"# not a comment\")\n",
        "p(\"a\nb\")\n",
    ];
    // The `stack NAME[keys]` header the keyed-stacks ticket brings to the
    // compiler: tree-sitter reads it already, and agrees once the
    // compiler does.
    let ahead = [
        "stack app[env] {}\n",
        "stack renfry.app[env, region] { backend = \"x\" }\n",
    ];
    for src in ahead {
        let errors = ts_errors(&ts_parse(src));
        assert!(errors.is_empty(), "{src:?}: {errors:?}");
    }
    let compiled = ahead.into_iter().filter(|s| parse(s).errors.is_empty());
    let mut bad = Vec::new();
    for src in snippets.into_iter().chain(compiled) {
        let tree = ts_parse(src);
        let compiler = parse(src).errors;
        let ts = ts_errors(&tree);
        if compiler.is_empty() != ts.is_empty() {
            bad.push(format!(
                "{src:?}: compiler {:?}, tree-sitter {ts:?}",
                compiler.iter().map(|e| &e.message).collect::<Vec<_>>()
            ));
        } else if compiler.is_empty() && rowan_shape(src) != ts_shape(&tree) {
            bad.push(format!(
                "{src:?}: the trees differ\n  compiler:    {:?}\n  tree-sitter: {:?}\n  {}",
                rowan_shape(src),
                ts_shape(&tree),
                tree.root_node().to_sexp()
            ));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
