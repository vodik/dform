//! A let with parameters (R-187): `let f(a, b) = t` is the relation
//! `f(a, b, v)` with the mode `(+, +, -)`, answered by the program where it
//! is called. A call `f(x, y)` in a term is its value; `f(x, y, v)` after
//! `where` is the same read; named arguments and defaults are std
//! functions'; a `+` argument left unbound is R-10's error, `in` over it,
//! a read of a data source inside it and a call of itself are errors. On
//! the mock.

mod common;
mod lsp_client;

use common::Scratch;

fn plan(src: &str) -> common::Run {
    let s = Scratch::new("lets-params");
    s.write("p.df", src);
    s.run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
}

/// The plan's `cidr = ..` lines, in order.
fn cidrs(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("cidr = "))
        .collect()
}

/// The reviewer's backups.df on the mock: the restic container is spelled
/// once, a let of three parameters, and each component's CronJob calls it
/// in its literal, one positionally and one by name; each copy's
/// container is its own.
const BACKUPS: &str = r#"input namespace: string
let image = "docker.io/restic/restic:0.18.0"
let run = "restic backup --tag $TAG $SOURCE"
let restic(tag, source, mount) = {
  name: "restic",
  image,
  command: ["sh", "-c", run],
  env: [{ name: "TAG", value: tag }, { name: "SOURCE", value: source }],
  volumeMounts: [{ name: mount, mountPath: source, readOnly: true }],
}
component volume {
  input name: string
  resource k8s.cron_job job {
    metadata = { name: "backup-${name}", namespace }
    spec.schedule = "0 3 * * *"
    spec.jobTemplate.spec.template.spec = {
      restartPolicy: "Never",
      containers: [restic(name, "/data", "data")],
    }
  }
}
component database {
  input name: string
  resource k8s.cron_job job {
    metadata = { name: "backup-${name}", namespace }
    spec.schedule = "0 4 * * *"
    spec.jobTemplate.spec.template.spec = {
      restartPolicy: "Never",
      initContainers: [{ name: "dump", image: "postgres" }],
      containers: [restic(tag: name, source: "/dump", mount: "dump")],
    }
  }
}
"#;

#[test]
fn the_reviewers_two_containers_are_spelled_once() {
    let s = Scratch::project("lets-params-backups");
    s.write("backups.df", BACKUPS);
    s.write(
        "main.df",
        "use fake\nuse k8s\nuse backups { namespace = \"apps\" }\n\
         resource backups.volume forgejo { name = \"forgejo\" }\n\
         resource backups.database synapse { name = \"synapse\" }\n",
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(r.summary(), "plan: 2 changes (2 create)", "{}", r.stdout);
    let c = "spec.jobTemplate.spec.template.spec.containers[name=restic]";
    for (job, tag, path, mount) in [
        ("forgejo", "forgejo", "/data", "data"),
        ("synapse", "synapse", "/dump", "dump"),
    ] {
        let at = r
            .stdout
            .find(&format!("+ k8s.cron_job[\"{job}.job\"]"))
            .unwrap_or_else(|| panic!("{job}\n{}", r.stdout));
        let block = &r.stdout[at..];
        let block = &block[..block[1..].find("\n+ ").map_or(block.len(), |i| i + 1)];
        for line in [
            format!("{c}.env[name=TAG].value = \"{tag}\""),
            format!("{c}.env[name=SOURCE].value = \"{path}\""),
            format!("{c}.image = \"docker.io/restic/restic:0.18.0\""),
            format!("{c}.command[2] = \"restic backup --tag $TAG $SOURCE\""),
            format!("{c}.volumeMounts[mountPath={path}].name = \"{mount}\""),
        ] {
            assert!(block.contains(&line), "{line}\n{block}");
        }
    }
}

/// Named arguments in any order and a parameter's default, as std
/// functions take them (R-155).
#[test]
fn named_arguments_and_a_default() {
    let r = plan(
        r#"use fake
let cidr(a, b: int = 7) = "10.${a}.${b}.0/24"
resource net.vpc u { cidr = cidr(1) }
resource net.vpc v { cidr = cidr(b: 3, a: 2) }
resource net.vpc w { cidr = cidr(4, 5) }
"#,
    )
    .success();
    assert_eq!(
        cidrs(&r.stdout),
        ["\"10.1.7.0/24\"", "\"10.2.3.0/24\"", "\"10.4.5.0/24\""],
        "{}",
        r.stdout
    );
    let r = plan(
        r#"use fake
let cidr(a, b: int = 7) = "10.${a}.${b}.0/24"
resource net.vpc v { cidr = cidr(1, c: 2) }
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("`cidr` has no parameter `c`: its parameters are a, b"),
        "{}",
        r.stderr
    );
    let r = plan(
        r#"use fake
let cidr(a, b) = "10.${a}.${b}.0/24"
resource net.vpc v { cidr = cidr(1) }
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("`cidr`'s `b` is left out, and has no default: give it (`cidr(a, b)`)"),
        "{}",
        r.stderr
    );
}

/// `f(x, y, v)` after `where` reads the same relation, its parameters bound
/// by the body; a parameter's clause makes it partial, and a `not { }` in
/// it reads the parameters too.
#[test]
fn the_relational_spelling_and_a_clause() {
    let r = plan(
        r#"use fake
pair(1, 2)
blocked(3)
let cidr(a, b) = "10.${a}.${b}.0/24"
let size(n) = "small" where not { blocked(n) }
let size(n) = "big" where blocked(n)
resource net.vpc u { cidr = c } where pair(x, y), cidr(x, y, c)
resource net.vpc v { cidr = size(1) }
resource net.vpc w { cidr = size(3) }
"#,
    )
    .success();
    assert_eq!(
        cidrs(&r.stdout),
        ["\"10.1.2.0/24\"", "\"small\"", "\"big\""],
        "{}",
        r.stdout
    );
}

/// A `+` argument nothing binds is R-10's error at it, as a provider's
/// table's is.
#[test]
fn an_unbound_argument_is_the_binding_order_error() {
    let r = plan(
        r#"use fake
let cidr(a, b) = "10.${a}.${b}.0/24"
resource net.vpc u { cidr = c } where cidr(q, 2, c), q = c.len
"#,
    )
    .failure();
    assert!(
        r.stderr.contains(
            "p.df:3:44: `q` is unbound in this call of `cidr`; bind it with `=`, `in`, or a \
             relation first"
        ),
        "{}",
        r.stderr
    );
}

/// `in` enumerates nothing of a let with parameters, and it has no value
/// uncalled: each is an error that says to call it.
#[test]
fn in_over_it_and_a_bare_read_are_errors() {
    let r = plan(
        r#"use fake
let cidr(a, b) = "10.${a}.${b}.0/24"
resource net.vpc u { cidr = c } where c in cidr
"#,
    )
    .failure();
    assert!(
        r.stderr.contains(
            "p.df:3:44: `in` over `cidr`: a let with parameters is a relation whose arguments \
             must be bound, so nothing enumerates it"
        ) && r.stderr.contains(
            "call it with its arguments, `cidr(a, b)`; `x in cidr(a, b)` reads a list it returns"
        ),
        "{}",
        r.stderr
    );
    let r = plan(
        r#"use fake
let cidr(a, b) = "10.${a}.${b}.0/24"
resource net.vpc u { cidr = cidr }
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("`cidr` is a let with parameters: call it, `cidr(a, b)`"),
        "{}",
        r.stderr
    );
}

/// A read of a data source inside a let with parameters is an error at
/// the read naming the let: a document, the environment.
#[test]
fn a_reader_inside_is_an_error() {
    for (reads, what) in [
        (r#"yaml.decode(io.read("x.yaml"))"#, "a yaml document"),
        (r#"env.var("HOME")"#, "`env.var`"),
    ] {
        let r = plan(&format!(
            "use fake\nuse env\nlet f(n) = {{ n, v: {reads} }}\n\
             resource net.vpc u {{ cidr = \"10.0.0.0/16\", tags = f(1) }}\n"
        ))
        .failure();
        assert!(
            r.stderr.contains(&format!(
                "let f reads {what}: a let with parameters is pure, its value its arguments' \
                 alone"
            )) && r
                .stderr
                .contains("read it in a `let` of its own and pass the value to f as a parameter"),
            "{reads}\n{}",
            r.stderr
        );
    }
}

/// A let with parameters that calls itself, directly or through another,
/// is refused, the calls named.
#[test]
fn recursion_is_refused_naming_the_cycle() {
    let r = plan(
        r#"use fake
let f(n) = g(n)
let g(n) = f(n)
resource net.vpc u { cidr = f(1) }
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:2:1: let f calls itself: f calls g calls f"),
        "{}",
        r.stderr
    );
}

/// Each call is answered where it is written: a call reading an attribute
/// of a resource whose own attribute another call writes stratifies,
/// as the two values written out would.
#[test]
fn each_call_is_answered_at_its_site() {
    let r = plan(
        r#"use fake
let labels(app) = { app, team: "x" }
resource net.vpc a { cidr = "10.0.0.0/16", tags = labels("web") }
resource db.postgres b { name = "b", tags = labels(a.cidr) }
"#,
    )
    .success();
    assert!(
        r.stdout.contains("tags.app = \"10.0.0.0/16\"") && r.stdout.contains("tags.app = \"web\""),
        "{}",
        r.stdout
    );
}

/// `why` of a value through a call: the attribute's step is the call at
/// its site; the relation's rows are each the let's rule, at the call.
#[test]
fn why_shows_the_rule_and_its_site() {
    let s = Scratch::new("lets-params-why");
    s.write(
        "p.df",
        r#"use fake
let cidr(a, b) = "10.${a}.${b}.0/24"
resource net.vpc v { cidr = cidr(1, 2) }
"#,
    );
    let r = s
        .run(&["dev", "--world", "w.json", "why", "v.cidr", "p.df"])
        .success();
    assert!(r.stdout.contains("= cidr(1, 2)  p.df:3"), "{}", r.stdout);
    let r = s
        .run(&["dev", "--world", "w.json", "why", "cidr(a, b, v)", "p.df"])
        .success();
    for line in [
        "decl cidr(a: int, b: int, value: string)",
        "cidr(1, 2, \"10.1.2.0/24\")",
        "p.df:3  resource net.vpc v { cidr = cidr(1, 2) }",
        "cidr(1, 2, \"10.1.2.0/24\")   (in call at p.df:3:29)",
        "p.df:2  let cidr(a, b) = \"10.${a}.${b}.0/24\"",
    ] {
        assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
    }
}

/// A typed parameter reads its argument as its type, and a literal that
/// is not one is an error at the argument.
#[test]
fn a_typed_parameter_reads_its_argument() {
    let r = plan(
        r#"use fake
let sub(n: inet, i: int) = inet.subnet(n, 8, i)
resource net.vpc u { cidr = sub("10.0.0.0/16", 3) }
"#,
    )
    .success();
    assert_eq!(cidrs(&r.stdout), ["\"10.0.3.0/24\""], "{}", r.stdout);
    let r = plan(
        r#"use fake
let sub(n: inet, i: int) = inet.subnet(n, 8, i)
resource net.vpc u { cidr = sub("10.0.0.0/16", "x") }
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:3:48: `sub`'s `i` is int, not the string \"x\""),
        "{}",
        r.stderr
    );
}

/// `dform fmt` writes a let's parameters as an extern's columns: hugging
/// its name, one per line past the width; and reads its own output back.
#[test]
fn the_formatter_round_trips() {
    let s = Scratch::new("lets-params-fmt");
    s.write(
        "p.df",
        "use fake\nlet cidr( a ,b:int=7 )= \"10.${a}.${b}.0/24\"\n\
         let long(alpha_parameter, beta_parameter, gamma_parameter, delta_parameter: string = \"a default value\") = alpha_parameter\n\
         resource net.vpc v { cidr = cidr(1, b : 2) }\n",
    );
    s.run(&["fmt", "p.df"]).success();
    assert_eq!(
        s.read("p.df"),
        "use fake\nlet cidr(a, b: int = 7) = \"10.${a}.${b}.0/24\"\nlet long(\n  \
         alpha_parameter,\n  beta_parameter,\n  gamma_parameter,\n  delta_parameter: string = \
         \"a default value\"\n) = alpha_parameter\nresource net.vpc v { cidr = cidr(1, b: 2) }\n"
    );
    s.run(&["fmt", "--check", "p.df"]).success();
}

/// The language server: hover on a call is the let and its columns as
/// inferred; signature help names its parameters.
#[test]
fn hover_and_signature_help_on_a_call() {
    let s = Scratch::project("lets-params-lsp");
    s.write(
        "main.df",
        "use fake\n#| A network by its two octets.\nlet cidr(a, b: int = 7) = \"10.${a}.${b}.0/24\"\n\
         resource net.vpc v { cidr = cidr(1, 2) }\n",
    );
    let root = std::fs::canonicalize(&s.dir).unwrap();
    let file = root.join("main.df");
    let mut c = lsp_client::Client::start(&root, serde_json::json!({}));
    c.open(&file);
    let hover = c.at(
        "textDocument/hover",
        &file,
        lsp_client::find(&file, "cidr(1, 2)", 1),
    );
    let text = hover["contents"]["value"].as_str().unwrap_or_default();
    for line in [
        "let cidr(a, b: int = 7)",
        "A network by its two octets.",
        "cidr(a: int, b: int, value: string)",
    ] {
        assert!(text.contains(line), "{line}\n{hover}");
    }
    let help = c.at(
        "textDocument/signatureHelp",
        &file,
        lsp_client::find(&file, "cidr(1, 2)", 8),
    );
    assert_eq!(
        help["signatures"][0]["label"], "cidr(a, b: int = 7)",
        "{help}"
    );
    assert_eq!(help["activeParameter"], 1, "{help}");
    c.shutdown();
}
