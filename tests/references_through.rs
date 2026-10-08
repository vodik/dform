//! A reference is a reference everywhere (R-185's follow-up): a read
//! through an attribute the schema types `ref(T)` reads the resource it
//! names, a reference compares as a reference, never as its address, and
//! a `set` through a resource of any type reads a quantity at the
//! attribute it writes.

mod common;
use common::{Scratch, mock};

/// A program under the fake provider.
fn scratch(name: &str, src: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", &format!("\nuse fake\n\n{src}"));
    s
}

/// Two networks and a subnet of the first.
const NETS: &str = r#"
resource net.vpc main {
  cidr = "10.0.0.0/16"
}
resource net.vpc other {
  cidr = "10.1.0.0/16"
}
resource net.subnet a {
  vpc = main
  cidr = "10.0.1.0/24"
}
"#;

/// The reviewer's shape: `s.vpc.cidr` in a deny reads the cidr of the
/// network the subnet names, where it was an error at the rule ("a
/// reference, which has no field `cidr`"); `why` shows the read.
#[test]
fn a_deny_reads_through_a_reference_attribute() {
    let s = scratch(
        "refs-through-deny",
        &format!(
            "{NETS}\n\
             deny \"in a /16\" {{ subnet: s }} where s in net.subnet, s.vpc.cidr == \"10.0.0.0/16\"\n\
             deny \"in other\" {{ subnet: s }} where s in net.subnet, s.vpc.cidr == \"10.1.0.0/16\"\n"
        ),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("- in a /16  subnet = \"a\"\n"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("in other"), "{}", r.stderr);
    let r = mock(&s, &["why", "deny \"in a /16\""]).success();
    assert!(
        r.stdout.contains("net.vpc main.cidr = \"10.0.0.0/16\""),
        "{}",
        r.stdout
    );
}

/// A chain of references is read through hop by hop: a node pool's
/// cluster, its subnets, each subnet's network.
#[test]
fn a_chain_of_references_is_read_through() {
    let s = scratch(
        "refs-through-chain",
        &format!(
            "{NETS}\n\
             resource k8s.cluster c {{\n  subnets = [a]\n}}\n\
             resource k8s.nodepool pool {{\n  cluster = c\n}}\n\
             deny \"wide\" {{ pool: p }} where p in k8s.nodepool, s in p.cluster.subnets, \
             s.vpc.cidr == \"10.0.0.0/16\"\n\
             deny \"narrow\" {{ pool: p }} where p in k8s.nodepool, \
             p.cluster.subnets[0].vpc.cidr == \"10.1.0.0/16\"\n"
        ),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("- wide  pool = \"pool\"\n"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("narrow"), "{}", r.stderr);
}

/// A reference to a resource no rule wants is the located error at the
/// attribute that holds it (R-194), so a read through it never answers
/// nothing.
#[test]
fn a_reference_to_a_resource_not_wanted_is_located() {
    let s = scratch(
        "refs-through-unwanted",
        "resource net.vpc gone {\n  cidr = \"10.0.0.0/16\"\n} where 1 == 2\n\
         resource net.subnet a {\n  vpc = gone\n  cidr = \"10.0.1.0/24\"\n}\n\
         deny \"wide\" { subnet: s } where s in net.subnet, s.vpc.cidr == \"10.0.0.0/16\"\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr
            .contains("net.vpc gone answered nothing, so net.subnet a.vpc has no value"),
        "{}",
        r.stderr
    );
}

/// A read through a reference is typed by the attribute it reads: the
/// network's `cidr` is a string, so comparing it with an int is an error
/// as a direct read's is, where it was a deny that always held.
#[test]
fn a_read_through_a_reference_is_typed_by_the_schema() {
    let s = scratch(
        "refs-through-typed",
        &format!("{NETS}\ndeny \"odd\" {{ subnet: s }} where s in net.subnet, s.vpc.cidr != 5\n"),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr
            .contains("`vpc.cidr != 5` compares string with int: they are never equal"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &s.read("p.df")
            .replace("s.vpc.cidr != 5", "c = s.vpc.cidr, c == 5"),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("is string, not the int 5"),
        "{}",
        r.stderr
    );
}

/// `s.vpc == v` with `v in net.vpc` holds for the same resource and `!=`
/// for another, and `v in [s.vpc]` is membership of the reference, where
/// a reference compared with an address string was never equal.
#[test]
fn references_compare_as_references() {
    let s = scratch(
        "refs-through-compare",
        &format!(
            "{NETS}\n\
             same(s, v) where s in net.subnet, v in net.vpc, s.vpc == v\n\
             other(s, v) where s in net.subnet, v in net.vpc, s.vpc != v\n\
             held(s, v) where s in net.subnet, v in net.vpc, v in [s.vpc]\n\
             deny \"same\" {{ v: v }} where same(_, v)\n\
             deny \"other\" {{ v: v }} where other(_, v)\n\
             deny \"held\" {{ v: v }} where held(_, v)\n"
        ),
    );
    let r = mock(&s, &["plan"]).failure();
    for (deny, v) in [("same", "main"), ("other", "other"), ("held", "main")] {
        assert!(
            r.stderr.contains(&format!("- {deny}  v = \"{v}\"\n")),
            "{deny}: {}",
            r.stderr
        );
    }
    assert!(!r.stderr.contains("same  v = \"other\""), "{}", r.stderr);
    assert!(!r.stderr.contains("other  v = \"main\""), "{}", r.stderr);
}

/// A reference compared with a string is never equal: a compile error
/// where the schema types the attribute, with the resource to write (or
/// `has`, for the empty string), where it was a deny that never fired.
#[test]
fn a_reference_compared_with_a_string_is_a_compile_error() {
    let s = scratch(
        "refs-through-string",
        &format!(
            "{NETS}\ndeny \"main\" {{ subnet: s }} where s in net.subnet, s.vpc == \"main\"\n"
        ),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "`s.vpc == \"main\"` compares a reference, ref(net.vpc), with the string \"main\": \
             a reference is never a string"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("compare with the resource: `net.vpc[\"main\"]`, or its name in scope"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &s.read("p.df")
            .replace("s.vpc == \"main\"", "not s.vpc == \"\""),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("`has s.vpc` tests whether it is set"),
        "{}",
        r.stderr
    );
}

/// Where nothing types the reference before evaluation (a resource of any
/// type), comparing it with a string is an error at the rule, never a
/// literal that silently does not hold: `==`, `!=` and `in` alike.
#[test]
fn a_reference_compared_with_a_string_at_run_time_is_an_error() {
    for (body, written) in [
        ("y == \"main\"", "`y == \"main\"`"),
        ("y != \"main\"", "`y != \"main\"`"),
        ("\"main\" in [y]", "`\"main\" in [y]`"),
    ] {
        let s = scratch(
            "refs-through-run-time",
            &format!(
                "{NETS}\ndeny \"main\" {{ r: x }} where x in resource, has x.vpc, y = x.vpc, {body}\n"
            ),
        );
        let r = mock(&s, &["plan"]).failure();
        assert!(
            r.stderr.contains(&format!(
                "{written} compares a reference, net.vpc main, with the string \"main\""
            )),
            "{body}: {}",
            r.stderr
        );
    }
}

/// The direct join where the type is a variable, `x in resource, x.vpc
/// == "main"`, reads the cell by its string: a reference there is the
/// same error at the rule, where it was a deny that never held. A string
/// cell still joins, and `not x.vpc == ..` keeps its meaning: no cell
/// equal to the string.
#[test]
fn a_direct_join_with_a_string_at_run_time_is_an_error() {
    let s = scratch(
        "refs-through-join",
        &format!("{NETS}\ndeny \"main\" {{ r: x }} where x in resource, x.vpc == \"main\"\n"),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "`x.vpc == \"main\"` compares a reference, net.vpc main, with the string \"main\""
        ),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &(s.read("p.df")
            .replace("x.vpc == \"main\"", "has x.vpc, not x.vpc == \"main\"")
            + "deny \"cidr\" { r: x } where x in resource, x.cidr == \"10.1.0.0/16\"\n"),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(r.stderr.contains("- main  r = \"a\"\n"), "{}", r.stderr);
    assert!(r.stderr.contains("- cidr  r = \"other\"\n"), "{}", r.stderr);
}

/// The reviewer's shape: a `set` of `cpu: 100m` through `x in resource`
/// reads the quantity as the attribute it writes takes it (cpu), where it
/// was "this position has no type".
#[test]
fn a_set_through_any_resource_reads_a_quantity_at_its_edge() {
    let s = Scratch::new("refs-through-quantity");
    s.write(
        "p.df",
        "\nuse fake\nuse k8s\n\n\
         resource k8s.deployment web {\n  metadata.name = \"web\"\n  \
         spec.selector.matchLabels = { app: \"web\" }\n  \
         spec.template.spec.containers = [{ name: \"web\", image: \"nginx:1\" }]\n}\n\
         set x.spec.template.spec.containers[_].resources.requests = {\n  \
         cpu: 100m,\n  memory: 128Mi,\n} @default where x in resource\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "spec.template.spec.containers[name=web].resources.requests = \
             { cpu: \"100m\", memory: \"128Mi\" }"
        ),
        "{}",
        r.stdout
    );
    // Where no type's attribute reads it, it stays the error.
    s.write(
        "p.df",
        &s.read("p.df").replace("requests = {", "requests.extra = {"),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("this position has no type"),
        "{}",
        r.stderr
    );
}

/// The reviewer's baseline.df shape: a module's `set` of each container's
/// requests through `x in resource`, guarded by `has` on the list it
/// walks. The schema answers the guard in place of its read, and the walk
/// read through that read: "unsafe member: list is not ground". The walk
/// reads the list itself; a type without the list is untouched.
#[test]
fn a_set_through_any_resource_under_a_has_guard_walks_the_list() {
    let s = Scratch::project("refs-through-has-walk");
    s.write(
        "baseline.df",
        "\nset x.spec.template.spec.containers[_].resources.requests = {\n  \
         cpu: 100m,\n  memory: 128Mi,\n} @default where x in resource, \
         has x.spec.template.spec.containers\n",
    );
    s.write(
        "p.df",
        "\nuse fake\nuse k8s\nuse baseline\n\n\
         resource k8s.deployment web {\n  metadata.name = \"web\"\n  \
         spec.selector.matchLabels = { app: \"web\" }\n  \
         spec.template.spec.containers = [{ name: \"web\", image: \"nginx:1\" }]\n}\n\
         resource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n}\n",
    );
    let r = mock(&s, &["plan", "--why=none"]).success();
    assert!(
        r.stdout.contains(
            "  spec.template.spec.containers[name=web].resources.requests.cpu = \"100m\"\n  \
             spec.template.spec.containers[name=web].resources.requests.memory = \"128Mi\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("+ net.vpc[\"main\"]\n  cidr = \"10.0.0.0/16\"\napply"),
        "{}",
        r.stdout
    );
}

/// A relation a copy exports carries its references across the copy's
/// boundary: `blue.subnet(s, _)` has `s` as the resource, so `s.zone`
/// reads it with no `s in net.subnet` to type it again.
#[test]
fn an_exported_relation_carries_references() {
    let s = scratch(
        "refs-through-export",
        "component vnet {\n  input cidr: inet\n  resource net.vpc vpc { cidr }\n  \
         resource net.subnet \"s-${z}\" {\n    vpc\n    cidr = inet.subnet(cidr, 8, i)\n    \
         zone = z\n  } where az(z, i)\n  \
         subnet(s) where s in net.subnet\n  output subnet\n}\n\
         az(\"a\", 0)\nresource vnet blue { cidr = \"10.0.0.0/16\" }\n\
         deny \"zone\" { subnet: s } where blue.subnet(s), s.zone == \"a\"\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("- zone  subnet = \"blue.s-a\"\n"),
        "{}",
        r.stderr
    );
}
