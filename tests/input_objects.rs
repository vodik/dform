//! An object-typed module input given whole by a `use` block (R-181): a
//! literal, a `let`, or another stack's object output is one contribution
//! whose fields are the object's. A read of a deployment not applied yet
//! is a null, which has every type; once applied the module plans with
//! the object. A field the input does not have, or a value that is no
//! object, is a violation naming the input (and the field); none panics.

mod common;
use common::Scratch;

const BACKUPS: &str = r#"
input namespace: string
input destination: { bucket: string, region: string, endpoint: string, prefix: string }
resource net.vpc store { cidr = "10.9.0.0/16", name = destination.bucket }
"#;

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc edge { cidr = "10.0.0.0/16" }
output backup = { bucket: "b", region: "gra", endpoint: "e", prefix: "p" }
"#;

fn project(name: &str, apps: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("backups.df", BACKUPS);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", apps);
    s
}

const STORE: &str =
    "  + net.vpc backups.store  backups.df:4\n      cidr = \"10.9.0.0/16\"\n      name = \"b\"";

/// The review's shape: `destination = platform[env].backup`, with and
/// without a guard on the `use`. Before platform is applied the module
/// waits on it; after, it plans with the object's field.
#[test]
fn a_use_gives_an_object_input_another_stacks_object_output() {
    for (name, guard, header) in [
        (
            "input-obj-guard",
            " where backup",
            "input backup: bool = true\n",
        ),
        ("input-obj-bare", "", ""),
    ] {
        let s = project(
            name,
            &format!(
                "key env: enum(\"lab\", \"prod\") = \"lab\"\n{header}use fake\nuse stacks.platform\n\
                 let apps = \"apps\"\n\
                 use backups {{ namespace = apps, destination = platform[env].backup }}{guard}\n"
            ),
        );
        let r = s.run(&["plan", "apps"]).success();
        assert_eq!(
            r.summary(),
            "plan: 1 create after stacks.platform[env=lab] is applied",
            "{}",
            r.stdout
        );
        assert!(
            r.stdout
                .contains("waits on  stack stacks.platform[env=lab]"),
            "{}",
            r.stdout
        );
        s.run(&["why", "net.vpc backups.store", "apps"]).success();

        s.run(&["apply", "--yes", "platform"]).success();
        let r = s.run(&["plan", "apps"]).success();
        assert!(r.stdout.contains(STORE), "{}", r.stdout);
        let r = s.run(&["why", "net.vpc backups.store", "apps"]).success();
        assert!(r.stdout.contains("name = \"b\""), "{}", r.stdout);
    }
}

/// A literal object, a `let` of one, and the four leaves one by one give
/// the same plan.
#[test]
fn an_object_input_is_given_whole_or_leaf_by_leaf() {
    let obj = "{ bucket: \"b\", region: \"gra\", endpoint: \"e\", prefix: \"p\" }";
    for apps in [
        format!("use fake\nuse backups {{ namespace = \"a\", destination = {obj} }}\n"),
        format!("use fake\nlet d = {obj}\nuse backups {{ namespace = \"a\", destination = d }}\n"),
        "use fake\nuse backups { namespace = \"a\", destination.bucket = \"b\", \
         destination.region = \"gra\", destination.endpoint = \"e\", destination.prefix = \"p\" }\n"
            .to_string(),
    ] {
        let s = project("input-obj-whole", &apps);
        let r = s.run(&["plan", "apps"]).success();
        assert!(r.stdout.contains(STORE), "{apps}\n---\n{}", r.stdout);
    }
}

/// An object with a field the input does not have is a violation naming
/// the input and the field; a value that is no object names the fields.
#[test]
fn an_object_input_given_whole_with_an_unknown_field_or_no_object_is_a_violation() {
    let s = project(
        "input-obj-unknown",
        "use fake\nlet d = { bucket: \"b\", region: \"gra\", endpoint: \"e\", prefix: \"p\", \
         regoin: \"x\" }\nuse backups { namespace = \"a\", destination = d }\n",
    );
    let r = s.run(&["plan", "apps"]).failure();
    assert!(
        r.stderr
            .contains("- input backups.destination has no field regoin\n"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );

    s.write(
        "backups.df",
        &BACKUPS.replace("name = destination.bucket", "name = namespace"),
    );
    s.write(
        "stacks/apps.df",
        "use fake\nuse backups { namespace = \"a\", destination = \"s3://x\" }\n",
    );
    let r = s.run(&["plan", "apps"]).failure();
    assert!(
        r.stderr.contains(
            "- input backups.destination: s3://x is not an object \
             (its fields: bucket, region, endpoint, prefix)\n"
        ),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}
