//! A secret flows through a function as through a `let` (the user's
//! project, 2026-10-16: the cloud-init document lived inside the node
//! component as plain lets, a secret refused by a function's parameter
//! with a help that declassified the k3s token). A parameter with no type
//! or typed `secret?(T)` takes a secret or a public value, each call's
//! value secret where its arguments are; a parameter typed `T` refuses one
//! at the call. On the mock, playing a project's schema.

use crate::common;
use common::Scratch;

const CLOUD: &str = r#"
type_provider("cloud.vm", "cloud")
type_attr("cloud.vm", "id", "string", ["computed", "id"])
type_attr("cloud.vm", "name", "string", ["required"])
type_attr("cloud.vm", "user_data", "string", ["sensitive"])
"#;

fn project(main: &str) -> Scratch {
    let s = Scratch::project("secret-functions");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\ncloud = \"./cloud\"\n",
    );
    s.write("cloud/schema.df", CLOUD);
    s.write("main.df", main);
    s
}

const NODES: &str = r##"use cloud

let k3s_token = random.password("k3s")
let cloud_init(token: TYPE, host) = "#cloud-config\nk3s_token: ${token}\nhost: ${host}\n"

resource cloud.vm a { name = "a", user_data = cloud_init(k3s_token, "a") }
resource cloud.vm b { name = "b", user_data = cloud_init("public", "b") }
let init_c = cloud_init(k3s_token, "c")
let init_d = cloud_init("public", "d")
"##;

/// The cloud-init document built by a function from the k3s token is a
/// secret, written where a secret goes and labelled; the same function
/// called with a public string gives a public value, which prints.
#[test]
fn a_secret_flows_through_a_function_call_by_call() {
    let s = project(&NODES.replace("TYPE", "secret?(string)"));
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout.contains(
            "  + cloud.vm a  main.df:6\n      name = \"a\"\n      user_data = (sensitive)\n"
        ),
        "{}",
        r.stdout
    );
    let why = |name: &str| s.run(&["why", name, "main.df"]).success().stdout;
    assert!(
        why("init_c").starts_with("let init_c = (sensitive let.init_c)\n"),
        "{}",
        why("init_c")
    );
    assert!(
        why("init_d")
            .starts_with("let init_d = \"#cloud-config\\nk3s_token: public\\nhost: d\\n\"\n"),
        "{}",
        why("init_d")
    );
    // Its value is a secret: a public place refuses it, the public
    // call's goes there.
    let leak = NODES
        .replace("TYPE", "secret?(string)")
        .replace("name = \"b\"", "name = cloud_init(k3s_token, \"b\")");
    s.write("main.df", &leak);
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "error  a secret reaches cloud.vm .name, not marked sensitive in the schema  [E0304]"
        ),
        "{}",
        r.stderr
    );
    // A parameter with no type is the same.
    s.write("main.df", &NODES.replace("token: TYPE", "token"));
    s.run(&["plan", "main.df"]).success();
}

/// A parameter typed `T` takes no secret: the error is at the call, and
/// its help is the parameter's spelling, never a declassification.
#[test]
fn a_parameter_of_a_type_refuses_a_secret_at_the_call() {
    let s = project(&NODES.replace("TYPE", "string"));
    let r = s.run(&["plan", "main.df"]).failure();
    for want in [
        "error  a secret is given to `cloud_init`'s `token`, declared `string`, which takes no \
         secret  [E0304]\n  main.df:6  resource cloud.vm a { name = \"a\", user_data = \
         cloud_init(k3s_token, \"a\") }\n",
        "  help: declare it `token: secret?(string)`: it takes a secret or not, and the call's \
         value is secret where an argument is, as a `let`'s is; a parameter with no type does \
         too\n",
    ] {
        assert!(r.stderr.contains(want), "{want}\n---\n{}", r.stderr);
    }
    assert!(r.stderr.contains("main.df:8  let init_c"), "{}", r.stderr);
    assert!(!r.stderr.contains("declassify"), "{}", r.stderr);
    // `secret(T)` takes it too.
    s.write("main.df", &NODES.replace("TYPE", "secret(string)"));
    s.run(&["plan", "main.df"]).success();
}
