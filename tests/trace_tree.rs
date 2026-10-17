//! A run's span tree is a documented shape: docs/reference.md's "A run's
//! trace" table is its schema, read here, and the traces of a plan and an
//! apply on the fake, of a tree of two stacks planned and applied on a
//! bucket, and of the Kubernetes provider planning against a fake API
//! server are checked against it: one root, every span a row's (exactly
//! one), of its row's kind, under a span its row allows, carrying only
//! the attributes its row names, each once, and none named by a path.
//!
//! The docs' table is the one copy of the schema, and the test reads it:
//! a reader of the docs reads what is checked, and a row the code stops
//! matching fails here naming it. (A table generated from Rust would be
//! a second copy, and a generator and a check of it to keep.)

use crate::common;
use crate::k8s_api::{Api, kubeconfig};
use common::Scratch;
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// What a hole in a span's name stands for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Hole {
    /// `{n}`: a number.
    Number,
    /// `{command}`, `{deployment}`, `{provider}`: one word.
    Word,
    /// `{method}`: an HTTP method, in capitals.
    Method,
    /// `{asks}`: what a provider call asks.
    Asks,
    /// `{about}`: what a call is about, the rest of the name.
    About,
}

/// What a provider call asks, as its name says it.
const ASKS: [&str; 10] = [
    "handshake",
    "configure",
    "schema",
    "query",
    "read",
    "plan",
    "apply",
    "import",
    "reveal",
    "health",
];

impl Hole {
    fn named(hole: &str) -> Hole {
        match hole {
            "n" => Hole::Number,
            "command" | "deployment" | "provider" => Hole::Word,
            "method" => Hole::Method,
            "asks" => Hole::Asks,
            "about" => Hole::About,
            other => panic!("the schema's hole {{{other}}} is none the test knows"),
        }
    }

    /// Whether `word` (no space in it) fills the hole.
    fn fits(self, word: &str) -> bool {
        !word.is_empty()
            && match self {
                Hole::Number => word.chars().all(|c| c.is_ascii_digit()),
                Hole::Word | Hole::About => true,
                Hole::Method => word.chars().all(|c| c.is_ascii_uppercase()),
                Hole::Asks => ASKS.contains(&word),
            }
    }
}

/// A span's name as the schema writes it: text, and holes.
#[derive(Debug)]
enum Part {
    Text(String),
    Hole(Hole),
}

#[derive(Debug)]
struct Name(Vec<Part>);

impl Name {
    fn parse(written: &str) -> Name {
        let mut parts = Vec::new();
        let mut rest = written;
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}').expect("a hole closed") + open;
            if open > 0 {
                parts.push(Part::Text(rest[..open].to_string()));
            }
            parts.push(Part::Hole(Hole::named(&rest[open + 1..close])));
            rest = &rest[close + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_string()));
        }
        Name(parts)
    }

    fn matches(&self, name: &str) -> bool {
        fits(&self.0, name)
    }
}

/// Whether `name` is `parts`: a hole takes a word (`{about}` the rest).
fn fits(parts: &[Part], name: &str) -> bool {
    match parts.split_first() {
        None => name.is_empty(),
        Some((Part::Text(t), rest)) => name.strip_prefix(t.as_str()).is_some_and(|n| fits(rest, n)),
        Some((Part::Hole(Hole::About), rest)) => {
            assert!(rest.is_empty(), "{{about}} ends a name");
            Hole::About.fits(name)
        }
        Some((Part::Hole(h), rest)) => {
            let end = name.find(' ').unwrap_or(name.len());
            h.fits(&name[..end]) && fits(rest, &name[end..])
        }
    }
}

/// A row of the schema: the span as it is written, its name, the span
/// as `tracing` names it, its OpenTelemetry kind (OTLP's number), the
/// rows it may be under, and the attributes it may carry.
#[derive(Debug)]
struct Row {
    span: String,
    name: Name,
    tracing: String,
    kind: u64,
    under: BTreeSet<String>,
    attributes: BTreeSet<String>,
}

/// The run's span, the one root.
const ROOT: &str = "dform {command}";
/// A deployment's span: a run of one deployment has its steps under the
/// run's instead.
const DEPLOYMENT: &str = "deployment {deployment}";

/// The code spans of `cell`, outside its parentheses (an attribute's
/// values, said beside it, are not attributes).
fn code_spans(cell: &str) -> Vec<String> {
    let mut outside = String::new();
    let (mut depth, mut code) = (0usize, false);
    for c in cell.chars() {
        match c {
            '(' if !code => depth += 1,
            ')' if !code => depth = depth.saturating_sub(1),
            c => {
                code ^= c == '`';
                if depth == 0 {
                    outside.push(c);
                }
            }
        }
    }
    outside
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

impl Row {
    /// A row of the table: `| Span | Kind | Under | Attributes | What |`.
    fn parse(line: &str) -> Row {
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        assert_eq!(cells.len(), 5, "a row of five cells: {line}");
        let span = code_spans(cells[0])
            .pop()
            .unwrap_or_else(|| panic!("a span: {line}"));
        let kind = match cells[1].rsplit(' ').next() {
            Some("internal") => 1,
            Some("server") => 2,
            Some("client") => 3,
            other => panic!("a kind, not {other:?}: {line}"),
        };
        let under = match cells[2] {
            "(the root)" => BTreeSet::new(),
            u => code_spans(u).into_iter().collect(),
        };
        let tracing = code_spans(cells[1])
            .pop()
            .unwrap_or_else(|| panic!("its tracing name: {line}"));
        Row {
            name: Name::parse(&span),
            span,
            tracing,
            kind,
            under,
            attributes: code_spans(cells[3]).into_iter().collect(),
        }
    }

    /// Whether a span of this row may be under one of `parent`'s.
    fn allows(&self, parent: &str) -> bool {
        self.under.contains(parent) || (parent == ROOT && self.under.contains(DEPLOYMENT))
    }
}

/// The schema: the rows of docs/reference.md's "A run's trace" table.
struct Schema(Vec<Row>);

impl Schema {
    fn read() -> Schema {
        let text = std::fs::read_to_string(common::repo().join("docs/reference.md")).unwrap();
        let section = text
            .split_once("\n### A run's trace\n")
            .expect("the section")
            .1;
        let table = section
            .split_once("\n| Span | Kind | Under | Attributes | What |\n|---|---|---|---|---|\n")
            .expect("its table")
            .1;
        let rows: Vec<Row> = table
            .lines()
            .take_while(|l| l.starts_with('|'))
            .map(Row::parse)
            .collect();
        let spans: BTreeSet<&str> = rows.iter().map(|r| r.span.as_str()).collect();
        for r in &rows {
            for u in &r.under {
                assert!(spans.contains(u.as_str()), "{}: under {u}, no row", r.span);
            }
        }
        assert!(rows.iter().any(|r| r.span == ROOT && r.under.is_empty()));
        Schema(rows)
    }

    /// The row of the span named `name`: the one that matches it.
    fn row(&self, name: &str) -> Result<&Row, String> {
        let rows: Vec<&Row> = self.0.iter().filter(|r| r.name.matches(name)).collect();
        match rows[..] {
            [row] => Ok(row),
            [] => Err("no row".into()),
            _ => Err(format!(
                "rows {}",
                rows.iter()
                    .map(|r| r.span.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// What of the trace `spans` the schema does not allow, each said
    /// with the span's index (none: of the whole trace).
    fn deviations(&self, spans: &[Json]) -> Vec<(Option<usize>, String)> {
        let by_id: BTreeMap<&str, &Json> = spans
            .iter()
            .map(|s| (s["spanId"].as_str().unwrap(), s))
            .collect();
        let name = |s: &Json| {
            let name = s["name"].as_str().unwrap_or_default();
            pending(name).unwrap_or(name).to_string()
        };
        let mut out = Vec::new();
        let roots: Vec<String> = spans
            .iter()
            .filter(|s| s["parentSpanId"] == "")
            .map(name)
            .collect();
        if roots.len() != 1 {
            out.push((None, format!("{} roots: {roots:?}", roots.len())));
        }
        for (i, s) in spans.iter().enumerate() {
            let n = match common::attr(s, "addr") {
                Some(addr) => format!("{} ({addr})", name(s)),
                None => name(s),
            };
            let mut said = |why: String| out.push((Some(i), format!("{n:?}: {why}")));
            if name(s).contains('/') {
                said("named by a path".into());
            }
            let row = match self.row(&name(s)) {
                Ok(row) => row,
                Err(why) => {
                    said(why);
                    continue;
                }
            };
            if s["kind"] != row.kind {
                said(format!("kind {}, not {}", s["kind"], row.kind));
            }
            match s["parentSpanId"].as_str().unwrap_or_default() {
                "" if row.span == ROOT => {}
                "" => said("a root".into()),
                p => match by_id.get(p) {
                    None => said("its parent is not in the trace".into()),
                    Some(parent) => match self.row(&name(parent)) {
                        Ok(under) if row.allows(&under.span) => {}
                        Ok(under) => said(format!("under {}", under.span)),
                        Err(_) => said(format!("under {:?}", name(parent))),
                    },
                },
            }
            let mut seen = BTreeSet::new();
            for a in s["attributes"].as_array().into_iter().flatten() {
                let k = a["key"].as_str().unwrap_or_default();
                if !seen.insert(k) {
                    said(format!("{k} twice"));
                }
                if !row.attributes.contains(k) {
                    said(format!("carries {k}"));
                }
            }
        }
        out
    }
}

/// The name a span has in the schema though the code that names it,
/// another branch's, does not yet give it: a provider's start is named
/// by its executable's path in plugin/providers/start.rs (fanout/
/// pipeline's), `provider {provider} started` once it is renamed; then
/// this goes.
fn pending(name: &str) -> Option<&'static str> {
    let path = name.strip_prefix("provider /")?;
    path.ends_with(" started (spawn and handshake)")
        .then_some("provider (pending) started")
}

/// The trace at `path` holds the schema's tree: its deviations, none.
fn assert_tree(s: &Scratch, path: &str) {
    assert_spans(&common::spans(&s.path(path)), path);
}

/// The spans `spans` of the trace `path` are the schema's tree.
fn assert_spans(spans: &[Json], path: &str) {
    assert!(spans.len() > 10, "{path}: {} spans", spans.len());
    let deviations = Schema::read().deviations(spans);
    let deviating: BTreeSet<_> = deviations.iter().map(|(i, _)| i).collect();
    let said: Vec<&str> = deviations.iter().map(|(_, d)| d.as_str()).collect();
    assert!(
        deviations.is_empty(),
        "{path}: {} deviations from the schema (docs/reference.md, \"A run's trace\"), in {} \
         of its {} spans:\n{}",
        deviations.len(),
        deviating.len(),
        spans.len(),
        said.join("\n")
    );
}

/// `dform ARGS` in `s`, with `env`, its trace to `trace` there; its run.
fn traced(s: &Scratch, trace: &str, args: &[&str], env: &[(&str, &str)]) -> common::Run {
    let out = common::dform()
        .args(["--trace", trace])
        .args(args)
        .current_dir(&s.dir)
        .env("NO_COLOR", "1")
        .env("DFORM_WAIT_POLL_MS", "50")
        .env_remove("DFORM_TRACE")
        .env_remove("DFORM_LOG")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("DFORM_K8S_OFFLINE")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    common::Run::from(out)
}

/// The schema reads whole: each row's name has a hole the test knows, is
/// under rows it has, and the run's span is the one root.
#[test]
fn the_schema_reads_whole() {
    let schema = Schema::read();
    assert!(schema.0.len() > 30, "{} rows", schema.0.len());
    for (name, span) in [
        ("dform plan", ROOT),
        ("deployment stacks.apps[env=lab]", DEPLOYMENT),
        ("plan ovh.instance k3s.server", "{asks} {about}"),
        ("configure ovh", "{asks} {about}"),
        ("k8s PATCH", "{provider} {method}"),
        ("tick 3", "tick {n}"),
        ("provider k8s started", "provider {provider} started"),
    ] {
        assert_eq!(
            schema.row(name).map(|r| r.span.as_str()),
            Ok(span),
            "{name}"
        );
    }
    for name in [
        "fakecloud Plan",
        "dform-provider-k8s.wasm Handshake",
        "provider /opt/dform-provider-k8s started (spawn and handshake)",
        "k8s GET https://10.0.0.1/api",
    ] {
        assert!(schema.row(name).is_err() || name.contains('/'), "{name}");
    }
}

/// The spans as `tracing` names them that dform-core's `timing` lists
/// are the schema's.
#[test]
fn the_timing_modules_list_is_the_schemas() {
    let text =
        std::fs::read_to_string(common::repo().join("crates/dform-core/src/timing.rs")).unwrap();
    let listed: BTreeSet<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("//! - "))
        .filter_map(|l| l.split_once(':'))
        .flat_map(|(names, _)| code_spans(names))
        .collect();
    let schema: BTreeSet<String> = Schema::read().0.into_iter().map(|r| r.tracing).collect();
    assert_eq!(listed, schema);
}

/// The database's endpoint gates the vpc: tick 1 makes the database,
/// tick 2 waits for the world to have its endpoint, tick 3 makes the vpc.
const WAITS: &str = r#"
use fake
resource db.postgres d { size = 1 }
resource net.vpc v { cidr = "10.0.0.0/16" } where d.endpoint == "d.db.fake"
"#;

/// A plan and an apply on the fake: the run's steps, its ticks, the
/// second a wait, and their calls.
#[test]
fn a_plan_and_an_apply_on_the_fake_are_the_schemas_tree() {
    let s = Scratch::new("trace-tree-fake");
    s.write("p.df", WAITS);
    let dev = ["dev", "--world", "w.json"];
    traced(
        &s,
        "plan.jsonl",
        &[&dev[..], &["plan", "p.df"]].concat(),
        &[],
    )
    .success();
    assert_tree(&s, "plan.jsonl");
    let apply = [
        &dev[..],
        &[
            "apply",
            "--yes",
            "--chaos",
            r#"not-ready=db.postgres["d"].endpoint:3"#,
            "p.df",
        ],
    ]
    .concat();
    let r = traced(&s, "apply.jsonl", &apply, &[]).success();
    assert!(r.stdout.contains("\ntick 3  1 change\n"), "{}", r.stdout);
    assert_tree(&s, "apply.jsonl");
}

/// Two stacks, the second reading the first's output, on a bucket: an
/// apply of the tree (its lock, its state and audit writes, its outputs
/// published) and a plan of it (each deployment's span, the reader's
/// wait on the other's plan, the outputs read), and a listing of its
/// stacks.
#[test]
fn a_tree_on_a_bucket_is_the_schemas_tree() {
    let server = dform_s3::fake::Server::start();
    let s = Scratch::project("trace-tree-s3");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[defaults]\nbackend = 's3(\"dform-test\", \
             \"tree/{{stack}}\", {{endpoint = \"{}\", region = \"us-east-1\"}})'\n\n\
             [secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
            server.endpoint
        ),
    );
    s.write(
        "stacks/platform.df",
        "key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\nresource net.vpc edge { cidr = \
         \"10.0.0.0/16\" }\noutput ingress_ip = edge.cidr\n",
    );
    s.write(
        "stacks/apps.df",
        "key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\nuse stacks.platform\nresource \
         net.vpc rec {\n  cidr = \"10.1.0.0/16\"\n  name = platform[env].ingress_ip\n}\n",
    );
    let env = [
        ("DFORM_S3_ACCESS_KEY_ID", "fake"),
        ("DFORM_S3_SECRET_ACCESS_KEY", "fake"),
        ("DFORM_TEST_PASSPHRASE", "trace tree"),
    ];
    traced(&s, "apply.jsonl", &["apply", "--yes", "platform"], &env).success();
    assert_tree(&s, "apply.jsonl");
    traced(&s, "plan.jsonl", &["plan", "apps"], &env).success();
    assert_tree(&s, "plan.jsonl");
    traced(&s, "tree.jsonl", &["apply", "--yes", "apps"], &env).success();
    assert_tree(&s, "tree.jsonl");
    traced(&s, "list.jsonl", &["stack", "list"], &env).success();
    assert_tree(&s, "list.jsonl");
}

/// The Kubernetes provider (its executable) planning against a fake API
/// server: its requests through the host under its calls, a Plan's a
/// dry run, paced in their buckets.
#[test]
fn a_providers_requests_are_the_schemas_tree() {
    let (_api, url) = Api::start();
    let s = Scratch::project("trace-tree-k8s");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers.k8s]\npath = \"{}\"\ncredentials = \
             [\"kubeconfig:environment\"]\n",
            common::exe("dform-provider-k8s")
        ),
    );
    let demo = common::repo().join("tests/fixtures/k8s/stacks/k8s_demo.df");
    s.write("k8s_demo.df", &std::fs::read_to_string(demo).unwrap());
    let kc = kubeconfig(&s, &url);
    let args = common::on("k8s_demo.df", &[], &["plan"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    traced(
        &s,
        "plan.jsonl",
        &args,
        &[("KUBECONFIG", &kc), ("HOME", &s.dir.display().to_string())],
    )
    .success();
    let spans = common::spans(&s.path("plan.jsonl"));
    assert!(
        spans.iter().any(|s| common::attr(s, "dry_run").is_some()),
        "a Plan's request is a dry run"
    );
    assert_tree(&s, "plan.jsonl");
}

/// A trace file of the reader's (`DFORM_TRACE_TREE=FILE`, as `--trace`
/// writes it) checked against the schema, its deviations said.
#[test]
#[ignore = "a reader's trace: DFORM_TRACE_TREE=FILE"]
fn a_readers_trace_is_the_schemas_tree() {
    let path = std::env::var("DFORM_TRACE_TREE").expect("DFORM_TRACE_TREE=FILE");
    assert_spans(&common::spans(std::path::Path::new(&path)), &path);
}
