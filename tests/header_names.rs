//! A bare header name is always the literal name (R-76): a resource may be
//! named like a `let`, an input, a used module or a copy in scope, and a
//! read whose first name means both is the error, at the read. A name from
//! the clause is a string, `"${t}"`.

mod common;
use common::{Scratch, error, mock};

/// `resource net.vpc env` beside `let env` is the VPC named "env"; a bare
/// read of `env` names both and is an error naming the typed read.
#[test]
fn a_bare_name_shadowing_a_let_declares_and_its_read_is_ambiguous() {
    let s = Scratch::new("header-let");
    s.write(
        "p.df",
        "\n\nprovider fake\n\
         let env = \"dev\"\n\
         resource net.vpc env { cidr = \"10.0.0.0/16\" }\n\
         resource net.subnet s { vpc = net.vpc[\"env\"], cidr = \"10.0.1.0/24\" }\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.contains("  + net.vpc env  ") && r.stdout.contains("vpc = env"),
        "{}",
        r.stdout
    );
    let e = error(
        "let env = \"dev\"\n\
         resource net.vpc env { cidr = \"10.0.0.0/16\" }\n\
         resource net.subnet s { vpc = env, cidr = \"10.0.1.0/24\" }\n",
    );
    assert!(
        e.contains(
            "`env` names the let and the resource net.vpc[\"env\"]: read net.vpc[\"env\"], \
             or name one of them otherwise"
        ),
        "{e}"
    );
    let e = error(
        "input nodes: int = 3\n\
         resource net.vpc nodes { cidr = \"10.0.0.0/16\", size = nodes }\n",
    );
    assert!(
        e.contains("`nodes` names the input and the resource net.vpc[\"nodes\"]"),
        "{e}"
    );
}

/// A bare header name the clause binds is an error naming the string form.
#[test]
fn a_clause_bound_bare_name_is_an_error() {
    let e = error("tenant(\"a\")\nresource net.vpc t { cidr = \"10.0.0.0/16\" } where tenant(t)\n");
    assert!(
        e.contains(
            "`t` is bound by the clause, but a bare header name is the resource's literal \
             name: a name from the clause is a string, `\"${t}\"`"
        ),
        "{e}"
    );
    let s = Scratch::new("header-clause");
    s.write(
        "p.df",
        "\n\nprovider fake\n\
         tenant(\"a\")\n\
         resource net.vpc \"${t}\" { cidr = \"10.0.0.0/16\" } where tenant(t)\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(r.stdout.contains("  + net.vpc a  "), "{}", r.stdout);
}

/// `config.base_domain` is the module's item; any other read through
/// `config` names the module and the resource, and the resource reads by
/// its type.
#[test]
fn a_module_item_reads_the_module_and_another_read_is_ambiguous() {
    let s = Scratch::new("header-module");
    s.write("config.df", "let base_domain = \"example.org\"\n");
    s.write(
        "p.df",
        "\n\nprovider fake\n\
         use config\n\
         resource net.vpc config { cidr = \"10.0.0.0/16\", tags = { d: config.base_domain } }\n\
         resource net.subnet s { vpc = net.vpc[\"config\"], cidr = net.vpc[\"config\"].cidr }\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout
            .lines()
            .any(|l| l.starts_with("      tags.d = \"example.org\"  ")
                && l.ends_with("  config.df:1"))
            && r.stdout.contains("  + net.subnet s  ")
            && r.stdout
                .contains("p.df:6\n      cidr = \"10.0.0.0/16\"\n      vpc = config\n"),
        "{}",
        r.stdout
    );
    s.write(
        "p.df",
        "\n\nprovider fake\n\
         use config\n\
         resource net.vpc config { cidr = \"10.0.0.0/16\" }\n\
         resource net.subnet s { vpc = net.vpc[\"config\"], cidr = config.cidr }\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "`config` names the module and the resource net.vpc[\"config\"]: read \
             `config.base_domain` or net.vpc[\"config\"].cidr"
        ),
        "{}",
        r.stderr
    );
}

/// A copy's output reads the copy; another read through its name is
/// ambiguous with a resource of that name.
#[test]
fn an_instance_output_reads_the_copy() {
    let src = "component c {\n  output size: int = 3\n}\n\
               instance c main\n\
               resource net.vpc main { cidr = \"10.0.0.0/16\", tags = { n: \"${main.size}\" } }\n";
    let e = error(&format!(
        "{src}resource net.subnet s {{ vpc = net.vpc[\"main\"], cidr = main.cidr }}\n"
    ));
    assert!(
        e.contains(
            "`main` names the instance main of c and the resource net.vpc[\"main\"]: read \
             `main.size` or net.vpc[\"main\"].cidr"
        ),
        "{e}"
    );
    dform_core::parser::parse_file("t.df", &format!("\n{src}")).unwrap();
}
