//! A value's type is the edge it reaches (R-192): every literal is read at
//! the type of the position its value flows to, wherever it is written,
//! and inference unifies every edge a value flows through. `100m` is
//! millicores where a cpu is wanted and minutes where a duration is, and
//! `"10.0.0.0/8"` a network where an inet is, whether it is written in the
//! attribute, in a let the attribute reads, in a let with parameters, in a
//! relation's row, an input, an output, a call's argument or a comparison.
//! Uses that disagree are an error naming both; a literal no use types is
//! an error naming what would. One case per position, on the mock's k8s
//! schema (a container's `resources.requests.cpu` is a cpu, `memory`
//! bytes) and fake's (`routes.destination` an inet).

mod common;
use common::{Run, Scratch};

/// Plan `src` (its inputs, then `use fake`, `use k8s`, then the rest) in
/// a scratch project.
fn plan(src: &str) -> Run {
    let s = Scratch::project("typed-edges");
    let (inputs, rest): (Vec<&str>, Vec<&str>) = src.lines().partition(|l| l.starts_with("input "));
    let (inputs, rest) = (inputs.join("\n"), rest.join("\n"));
    s.write("main.df", &format!("{inputs}\nuse fake\nuse k8s\n{rest}\n"));
    s.run(&["plan", "--why=none", "main.df"])
}

/// A job whose one container is `container`.
fn job(container: &str) -> String {
    format!(
        "resource k8s.job j {{\n  metadata = {{ name: \"j\", namespace: \"a\" }}\n  \
         spec.template.spec = {{ restartPolicy: \"Never\", containers: [{container}] }}\n}}\n"
    )
}

/// The container `a` requesting `requests`.
fn requesting(requests: &str) -> String {
    job(&format!(
        "{{ name: \"a\", image: \"x\", resources: {{ requests: {requests} }} }}"
    ))
}

const CPU: &str = "containers[name=a].resources.requests.cpu = \"100m\"";
const MEMORY: &str = "containers[name=a].resources.requests.memory = \"128Mi\"";

/// The plan of `src` succeeds and says each of `lines`.
#[track_caller]
fn plans(src: &str, lines: &[&str]) {
    let r = plan(src).success();
    for l in lines {
        assert!(r.stdout.contains(l), "{l}\n{}", r.stdout);
    }
}

/// The plan of `src` fails saying each of `parts`.
#[track_caller]
fn fails(src: &str, parts: &[&str]) {
    let r = plan(src).failure();
    for p in parts {
        assert!(r.stderr.contains(p), "{p}\n{}", r.stderr);
    }
}

/// The attribute itself: the schema's type reads the literal (R-66).
#[test]
fn an_attribute_reads_its_literal() {
    plans(&requesting("{ cpu: 100m, memory: 128Mi }"), &[CPU, MEMORY]);
}

/// A let with a declared scalar type (R-74).
#[test]
fn a_typed_let_reads_its_literal() {
    plans(
        &format!("let c: cpu = 100m\n{}", requesting("{ cpu: c }")),
        &[CPU],
    );
}

/// A let with a declared object type reads each field at its type.
#[test]
fn a_typed_object_let_reads_its_fields() {
    plans(
        &format!(
            "let r: {{ cpu: cpu, memory: bytes }} = {{ cpu: 100m, memory: 128Mi }}\n{}",
            requesting("r")
        ),
        &[CPU, MEMORY],
    );
}

/// A field of a declared object type that is not its type is an error at
/// the let, for a value type read from a string too.
#[test]
fn a_typed_object_let_checks_its_fields() {
    let e = common::error("let hosts: { a: ip, b: uri } = { a: \"10.0.0\", b: \"https://x\" }\n");
    assert!(
        e.contains("let hosts.a is an ip: \"10.0.0\" is not an address"),
        "{e}"
    );
}

/// A list of a declared element type reads each element.
#[test]
fn a_typed_list_let_reads_its_elements() {
    let e = common::error("let nets: list(inet) = [\"10.0.0.0/8\", \"10.1\"]\n");
    assert!(e.contains("\"10.1\" is not a network"), "{e}");
    plans(
        &format!(
            "let reqs: list({{ cpu: cpu }}) = [{{ cpu: 100m }}]\nlet req = r where r in reqs\n{}",
            requesting("req")
        ),
        &[CPU],
    );
}

/// An untyped let is typed by the attribute it reaches.
#[test]
fn an_untyped_let_is_typed_by_its_use() {
    plans(
        &format!(
            "let limits = {{ cpu: 100m, memory: 128Mi }}\n{}",
            requesting("limits")
        ),
        &[CPU, MEMORY],
    );
}

/// A scalar let, and a field of an object let read where it is used.
#[test]
fn a_let_read_by_a_field_is_typed_by_its_use() {
    plans(
        &format!("let c = 100m\n{}", requesting("{ cpu: c }")),
        &[CPU],
    );
    plans(
        &format!(
            "let limits = {{ cpu: 100m }}\n{}",
            requesting("{ cpu: limits.cpu }")
        ),
        &[CPU],
    );
}

/// A let that reads a let: the edge reaches through both.
#[test]
fn a_let_through_a_let_is_typed_by_its_use() {
    plans(
        &format!(
            "let c = 100m\nlet limits = {{ cpu: c }}\n{}",
            requesting("limits")
        ),
        &[CPU],
    );
}

/// The reviewer's shape (backups.df): the container's resources written
/// in a let with parameters, its call's result in the attribute.
#[test]
fn a_let_with_parameters_is_typed_by_its_call() {
    plans(
        &format!(
            "let restic(tag) = {{\n  name: \"a\",\n  image: \"restic\",\n  \
             env: [{{ name: \"TAG\", value: tag }}],\n  \
             resources: {{ requests: {{ cpu: 100m, memory: 128Mi }} }},\n}}\n{}",
            job("restic(\"x\")")
        ),
        &[CPU, MEMORY],
    );
}

/// A literal argument of a let with parameters: the parameter's use types
/// it.
#[test]
fn an_argument_is_typed_by_its_parameters_use() {
    plans(
        &format!("let box(c) = {{ cpu: c }}\n{}", requesting("box(100m)")),
        &[CPU],
    );
}

/// A let in a component, read by the component's resource, and the
/// stack's let read by a component (R-186).
#[test]
fn a_let_in_a_component_is_typed_by_its_use() {
    let src = |lets: &str, body: &str| {
        format!(
            "{lets}component box {{\n  input name: string\n{body}{}}}\n\
             resource box b {{ name = \"b\" }}\n",
            requesting("small")
        )
    };
    plans(&src("", "  let small = { cpu: 100m }\n"), &[CPU]);
    plans(&src("let small = { cpu: 100m }\n", ""), &[CPU]);
}

/// A component's typed input takes the literal its instance gives it, and
/// a let the instance gives it.
#[test]
fn a_typed_input_reads_what_its_instance_gives() {
    let component = format!(
        "component box {{\n  input c: cpu\n{}}}\n",
        requesting("{ cpu: c }")
    );
    plans(
        &format!("{component}resource box b {{ c = 100m }}\n"),
        &[CPU],
    );
    plans(
        &format!("{component}let lim = 100m\nresource box b {{ c = lim }}\n"),
        &[CPU],
    );
}

/// An object-typed input reads the fields its instance gives at their
/// types.
#[test]
fn an_object_input_reads_its_fields() {
    plans(
        &format!(
            "component box {{\n  input cfg: {{ cpu: cpu }}\n{}}}\n\
             resource box b {{ cfg = {{ cpu: 100m }} }}\n",
            requesting("cfg")
        ),
        &[CPU],
    );
}

/// An input's default is read as its type, and flows on.
#[test]
fn an_input_default_reads_its_literal() {
    plans(
        &format!("input c: cpu = 100m\n{}", requesting("{ cpu: c }")),
        &[CPU],
    );
}

/// A typed output reads its literal.
#[test]
fn a_typed_output_reads_its_literal() {
    plans("output c: cpu = 100m\n", &[]);
}

/// A let a typed let reads: the typed let's type is the edge.
#[test]
fn a_let_read_by_a_typed_let_is_typed_by_it() {
    plans(
        &format!(
            "let a = {{ cpu: 100m }}\nlet b: {{ cpu: cpu }} = a\n{}",
            requesting("b")
        ),
        &[CPU],
    );
}

/// A relation's row: the column's use types its literal.
#[test]
fn a_relations_row_is_typed_by_its_columns_use() {
    plans(
        &format!(
            "size(\"a\", 100m)\nlet c = x where size(\"a\", x)\n{}",
            requesting("{ cpu: c }")
        ),
        &[CPU],
    );
}

/// A declared relation's column reads its rows' literals.
#[test]
fn a_declared_column_reads_its_rows() {
    plans(
        &format!(
            "decl size(k: string, c: cpu)\nsize(\"a\", 100m)\n\
             let c = x where size(\"a\", x)\n{}",
            requesting("{ cpu: c }")
        ),
        &[CPU],
    );
}

/// A `set` through a variable, of a literal and of a let.
#[test]
fn a_set_reads_its_value_at_the_attribute() {
    let deployment = "resource k8s.deployment web {\n  metadata.name = \"web\"\n  \
                      spec.selector.matchLabels = { app: \"web\" }\n  \
                      spec.template.spec.containers = [{ name: \"a\", image: \"x\" }]\n}\n";
    let set = |v: &str| {
        format!(
            "{deployment}set d.spec.template.spec.containers[c.name].resources.requests = {v} \
             where {{ d in k8s.deployment, c in d.spec.template.spec.containers }}\n"
        )
    };
    plans(&set("{ cpu: 100m }"), &[CPU]);
    plans(
        &format!("let lim = {{ cpu: 100m }}\n{}", set("lim")),
        &[CPU],
    );
    // Through a variable of any type: each type that declares the path.
    let any = set("lim").replace("d in k8s.deployment", "d in resource");
    plans(&format!("let lim = {{ cpu: 100m }}\n{any}"), &[CPU]);
}

/// A comparison: the other side's type reads the literal.
#[test]
fn a_comparison_reads_its_literal_as_the_other_side() {
    let r = plan(&format!(
        "{}deny \"cpu above 50m\" where {{\n  j in k8s.job\n  \
         c in j.spec.template.spec.containers\n  c.resources.requests.cpu > 50m\n}}\n",
        requesting("{ cpu: 100m }")
    ));
    assert!(
        r.stdout.contains("cpu above 50m") || r.stderr.contains("cpu above 50m"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// A string read as a value type where a function's parameter takes one.
#[test]
fn a_function_parameter_reads_its_argument() {
    fails(
        "let base = \"10.0.0/16\"\nlet s = inet.subnet(base, 8, 1)\noutput o = \"${s}\"\n",
        &["\"10.0.0/16\" is not a network"],
    );
}

/// A string in a let read as the inet the attribute it reaches is, and
/// checked as one before anything is planned.
#[test]
fn a_string_in_a_let_is_read_at_its_edge() {
    let src = |cidr: &str| {
        format!(
            "resource net.vpc v {{ cidr = \"10.0.0.0/16\" }}\n\
             let route = {{ destination: \"{cidr}\", target: v }}\n\
             resource net.route_table t {{\n  vpc = v\n  routes = [route]\n}}\n"
        )
    };
    plans(
        &src("10.1.0.0/16"),
        &["routes[0].destination = \"10.1.0.0/16\""],
    );
    fails(&src("10.1"), &["let route", "\"10.1\" is not a network"]);
}

/// Two uses that read a literal differently are an error naming both.
#[test]
fn uses_that_disagree_are_an_error_naming_both() {
    fails(
        &format!(
            "let t = 100m\noutput d: duration = t\n{}",
            requesting("{ cpu: t }")
        ),
        &["`100m` in `let t`", "cpu", "duration"],
    );
}

/// A literal no use types is the error naming the let and what would type
/// it.
#[test]
fn a_literal_no_use_types_names_the_let() {
    fails(
        "let limits = { cpu: 100m }\n",
        &["`100m` in `let limits`", "give the let a type"],
    );
}

/// An unambiguous literal needs no edge.
#[test]
fn an_unambiguous_literal_needs_no_edge() {
    plans("let m = { memory: 128Mi, name: \"x\" }\n", &[]);
}

/// A literal with a spread (R-199): the written fields keep the edge's
/// type, and a let spread there is typed by it too (the reviewer's
/// `{ ..restic(..), resources: { cpu: 100m } }`).
#[test]
fn a_spread_keeps_the_edges_type() {
    plans(
        &format!(
            "let base = {{ cpu: 100m }}\n\
             let restic(tag) = {{ name: \"a\", image: \"x\", env: [{{ name: \"TAG\", value: tag }}] }}\n{}",
            job(
                "{ ..restic(\"t\"), resources: { requests: { ..base, memory: 128Mi }, limits: { cpu: 100m } } }"
            )
        ),
        &[
            CPU,
            MEMORY,
            "containers[name=a].resources.limits.cpu = \"100m\"",
        ],
    );
}

/// An element of a literal list (`x in [..]`), an element by index, and
/// a comprehension's: each is the list's element.
#[test]
fn an_element_is_typed_by_its_use() {
    plans(
        &format!("let c = x where x in [100m]\n{}", requesting("{ cpu: c }")),
        &[CPU],
    );
    plans(
        &format!(
            "let cs = [{{ cpu: 100m }}]\nlet reqs = [r | r in cs]\n{}",
            requesting("reqs[0]")
        ),
        &[CPU],
    );
}

/// An object's entry (`(k, v) in o`) and an object pattern's field.
#[test]
fn an_entry_and_a_pattern_are_typed_by_their_use() {
    plans(
        &format!(
            "let lim = {{ cpu: 100m }}\nlet c = v where (_, v) in lim\n{}",
            requesting("{ cpu: c }")
        ),
        &[CPU],
    );
    plans(
        &format!(
            "let lim = {{ cpu: 100m }}\nlet c = x where {{\n  {{ cpu: x }} = lim\n}}\n{}",
            requesting("{ cpu: c }")
        ),
        &[CPU],
    );
}

/// `a + b`: each side takes the other's type, and the result's.
#[test]
fn a_sums_sides_are_typed_by_each_other() {
    plans(
        &format!(
            "let base: cpu = 50m\nlet c = base + 50m\n{}",
            requesting("{ cpu: c }")
        ),
        &[CPU],
    );
    plans(
        &format!("let c = 50m + 50m\n{}", requesting("{ cpu: c }")),
        &[CPU],
    );
}

/// The plan's `-vv` says where a literal's type came from: the edge it
/// reached (the use site).
#[test]
#[ignore = "the plan's `why` does not carry a literal's edge yet; the report is another ticket's (report/**, R-200)"]
fn the_plan_says_where_a_literals_type_came_from() {
    let s = Scratch::project("typed-edges-why");
    s.write(
        "main.df",
        &format!(
            "use fake\nuse k8s\nlet limits = {{ cpu: 100m }}\n{}",
            requesting("limits")
        ),
    );
    let r = s.run(&["plan", "-vv", "main.df"]).success();
    assert!(
        r.stdout.contains("cpu, where it reaches k8s.job[\"j\"]"),
        "{}",
        r.stdout
    );
}

/// Another deployment's typed output types the literal its reader
/// compares it with.
#[test]
#[ignore = "a keyed read of another deployment's output is typed at run time; its declaration is not an edge of the reader's program"]
fn another_deployments_output_types_its_readers_literal() {
    let s = Scratch::project("typed-edges-stacks");
    s.write("stacks/platform.df", "use fake\noutput limit: cpu = 100m\n");
    s.write(
        "stacks/apps.df",
        "use fake\nuse stacks.platform\nwarn \"limit above 50m\" where platform.limit > 50m\n",
    );
    s.run(&["apply", "platform"]).success();
    s.run(&["plan", "apps"]).success();
}

/// A coeffect's argument (`oci.resolve`) is checked as its parameter's
/// type at compile time, as a pure function's is.
#[test]
#[ignore = "a coeffect's row is keyed by its argument as written: read at compile time, the evaluated call's key differs from the one the host answered (files/oci.rs), so its arguments take no edge yet"]
fn a_coeffects_argument_is_checked_at_compile_time() {
    fails(
        &job("{ name: \"a\", image: oci.resolve(\"Not An Image\") }"),
        &["is an oci"],
    );
}
