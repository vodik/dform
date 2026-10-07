//! Quantities (R-66): `bytes` and `cpu` (and `duration`, R-62) are values
//! with a unit, parsed at the edge, compared and summed in base units,
//! printed canonically and sent to each provider in its schema's render
//! form. The compile-time errors are tests/syntax/err/quantities.df's.

mod common;
use common::{Scratch, query};

fn program(body: &str) -> String {
    format!("\n\n{body}\nuse fake\n")
}

/// Every unit of a dimension compares in its base unit, sums and takes a
/// min and max there, and prints in the largest unit that divides.
#[test]
fn quantities_compare_and_aggregate_across_units() {
    let s = Scratch::new("q-compare");
    s.write(
        "p.df",
        &program(
            "let two: cpu = 2\n\
             let quarter: cpu = 250m\n\
             let millis: cpu = 2000m\n\
             let some: cpu = 1500m\n\
             size(\"a\", 512Mi)\n\
             size(\"b\", 1Gi)\n\
             size(\"c\", 1.5Gi)\n\
             total(n) where n = sum(x), size(_, x)\n\
             least(n) where n = min(x), size(_, x)\n\
             most(n) where n = max(x), size(_, x)\n\
             big(k) where size(k, x), x > 1000Mi\n\
             same() where 2048Mi == 2Gi\n\
             cores() where millis == two\n\
             half(x) where x = 1Gi / 2\n\
             ratio(r) where r = two / quarter\n\
             scaled(x) where x = 3 * 512Mi + 26Mi\n\
             label(l) where l = \"limit ${1536Mi}\"\n\
             text(t) where t = \"${some}\"\n\
             mib(n) where n = quantity.to(1.5Gi, \"Mi\")\n",
        ),
    );
    for (goal, want) in [
        ("total(n)", "3Gi"),
        ("least(n)", "512Mi"),
        ("most(n)", "1536Mi"),
        ("half(x)", "512Mi"),
        ("ratio(r)", "8"),
        ("scaled(x)", "1562Mi"),
        ("label(l)", "\"limit 1536Mi\""),
        ("text(t)", "\"1500m\""),
        ("mib(n)", "1536"),
    ] {
        let r = query(&s, goal).success();
        assert!(
            r.stdout.contains(&format!("\n{want}\n")),
            "{goal}: {}",
            r.stdout
        );
    }
    let r = query(&s, "big(k)").success();
    assert!(
        r.stdout.contains("\"b\"") && r.stdout.contains("\"c\"") && !r.stdout.contains("\"a\""),
        "{}",
        r.stdout
    );
    assert!(query(&s, "same()").success().stdout.contains("yes"));
    assert!(query(&s, "cores()").success().stdout.contains("yes"));
}

/// `m` is millicores in a cpu position and minutes in a duration one; in a
/// position with no type the literal is an error naming both readings.
#[test]
fn m_is_read_by_its_position_and_alone_is_an_error() {
    let s = Scratch::new("q-m");
    s.write(
        "p.df",
        "\n\ninput limit: cpu = 500m\ninput grace: duration = 90m\n\n\
         got(l, g) where l = limit, g = grace\nuse fake\n",
    );
    let r = query(&s, "got(l, g)").success();
    assert!(
        r.stdout.contains("500m") && r.stdout.contains("1h30m"),
        "{}",
        r.stdout
    );

    s.write("p.df", &program("bare(x) where x = 500m\n"));
    let r = query(&s, "bare(x)").failure();
    assert!(
        r.stderr.contains(
            "p.df:3:19: `500m` is millicores in a cpu position and minutes in a duration \
             position, and this position has no type: give it one, `let c: cpu = 500m` or `let d: \
             duration = 500m`"
        ),
        "{}",
        r.stderr
    );
}

/// A container's memory and cpu are typed by the k8s schema: the literal
/// is read as one (`500m` millicores), a policy compares it, and the
/// provider is sent Kubernetes's quantity string.
#[test]
fn a_policy_compares_memory_limits_in_bytes() {
    let s = Scratch::project("q-k8s");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    let src = |memory: &str| {
        format!(
            "\n\nuse k8s\n\n\
             resource k8s.deployment web {{\n  metadata.name = \"web\"\n  \
             spec.selector.matchLabels = {{ app: \"web\" }}\n  \
             spec.template.spec.containers = [{{\n    name: \"web\",\n    image: \"web:1\",\n    \
             resources: {{ requests: {{ cpu: 500m, memory: 1Gi }}, limits: {{ cpu: 2000m, memory: {memory} }} }}\n  \
             }}]\n}}\n\n\
             deny \"memory limit above 2Gi\" {{ container: c.name }} where {{\n  \
             d in k8s.deployment\n  c in d.spec.template.spec.containers\n  \
             c.resources.limits.memory > 2Gi\n}}\n"
        )
    };
    s.write("main.df", &src("1536Mi"));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "main"])
        .success();
    for line in [
        "resources.limits.cpu = \"2\"",
        "resources.limits.memory = \"1536Mi\"",
        "resources.requests.cpu = \"500m\"",
        "resources.requests.memory = \"1Gi\"",
    ] {
        assert!(r.stdout.contains(line), "{line}: {}", r.stdout);
    }
    s.write("main.df", &src("3Gi"));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "main"])
        .failure();
    assert!(
        r.stdout.contains("memory limit above 2Gi") || r.stderr.contains("memory limit above 2Gi"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// The same `20Gi` renders per schema: RDS's allocated_storage takes whole
/// GiB (`20`), a claim's storage Kubernetes's string; a value that is not
/// a whole number of the unit is an error naming the attribute.
#[test]
fn one_spelling_renders_per_schema() {
    let s = Scratch::project("q-render");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\naws = { source = \"aws-mock\" }\nk8s = \"k8s\"\n",
    );
    let src = |storage: &str| {
        format!(
            "\n\nuse aws\nuse k8s\n\n\
             resource aws.db_instance db {{\n  instance_class = \"db.t3.micro\"\n  allocated_storage = {storage}\n}}\n\n\
             resource k8s.persistent_volume_claim data {{\n  metadata.name = \"data\"\n  \
             spec.accessModes = [\"ReadWriteOnce\"]\n  spec.resources.requests.storage = {storage}\n}}\n"
        )
    };
    s.write("main.df", &src("20Gi"));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "main"])
        .success();
    assert!(
        r.stdout.contains("  allocated_storage = 20\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("  spec.resources.requests.storage = \"20Gi\"\n"),
        "{}",
        r.stdout
    );
    s.write("main.df", &src("1536Mi"));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "main"])
        .failure();
    assert!(
        r.stderr.contains(
            "aws.db_instance[\"db\"].allocated_storage is sent to the provider in whole GiB, \
             and 1536Mi is not"
        ),
        "{}",
        r.stderr
    );
    // A decimal unit is an error at the literal, naming the binary one.
    s.write("main.df", &src("20GB"));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "main"])
        .failure();
    assert!(r.stderr.contains("write `20Gi`"), "{}", r.stderr);
}

/// A string in a quantity attribute is read as one, and a literal of
/// another dimension is an error at the attribute.
#[test]
fn a_quantity_attribute_reads_a_string_and_refuses_another_dimension() {
    let s = Scratch::project("q-attr");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    let src = |storage: &str| {
        format!(
            "\n\nuse k8s\n\n\
             resource k8s.persistent_volume_claim data {{\n  metadata.name = \"data\"\n  \
             spec.accessModes = [\"ReadWriteOnce\"]\n  spec.resources.requests.storage = {storage}\n}}\n\
             big(x) where c in k8s.persistent_volume_claim, x = c.spec.resources.requests.storage, x >= 1Gi\n"
        )
    };
    s.write("main.df", &src("\"1.5Gi\""));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "main"])
        .success();
    assert!(r.stdout.contains("storage = \"1536Mi\""), "{}", r.stdout);
    let q = s
        .run(&["dev", "--world", "w.json", "query", "big(x)", "main"])
        .success();
    assert!(q.stdout.contains("1536Mi"), "{}", q.stdout);
    s.write("main.df", &src("500m"));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "main"])
        .failure();
    assert!(
        r.stderr.contains(
            "k8s.persistent_volume_claim[\"data\"].spec.resources.requests.storage is bytes: \
             `500m` is not bytes"
        ),
        "{}",
        r.stderr
    );
}

/// `dform test` reads the literals as `plan` does: `500m` in a cpu
/// attribute is millicores, and a deny over memory fires.
#[test]
fn dform_test_reads_quantities_as_plan_does() {
    let s = Scratch::project("q-test");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write(
        "main.df",
        "\n\nuse k8s\n\n\
         resource k8s.deployment web {\n  metadata.name = \"web\"\n  \
         spec.selector.matchLabels = { app: \"web\" }\n  \
         spec.template.spec.containers = [{\n    name: \"web\",\n    image: \"web:1\",\n    \
         resources: { limits: { cpu: 500m, memory: 3Gi } }\n  }]\n}\n\n\
         deny \"memory limit above 2Gi\" { container: c.name } where {\n  \
         d in k8s.deployment\n  c in d.spec.template.spec.containers\n  \
         c.resources.limits.memory > 2Gi\n}\n",
    );
    let r = s.run(&["test", "main"]).failure();
    assert!(
        r.stdout.contains("memory limit above 2Gi") || r.stderr.contains("memory limit above 2Gi"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}
