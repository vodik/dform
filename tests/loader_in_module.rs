//! A loader's declaration is the compiler's (R-129): `yaml(..)` reads the
//! same wherever the call stands, a used module's resource clause, a
//! component a copy makes, a `let`, and no message names what it lowers
//! to. A provider's `use` stands in any module too: it configures the
//! provider for the deployment, and two that disagree are a conflict
//! naming both.

mod common;
use common::{Run, Scratch};

/// ConfigMaps as a vendored manifest lists them.
const MAPS: &str = "\
metadata:
  name: a
data:
  x: \"1\"
---
metadata:
  name: b
data:
  x: \"2\"
";

/// A project of the stack `platform` and the module `traefik` it uses.
fn project(name: &str, stack: &str, module: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("vendor/maps.yml", MAPS);
    s.write("stacks/platform.df", stack);
    s.write("traefik.df", module);
    s
}

/// What the run said holds no compiler word the program did not write.
#[track_caller]
fn plain(s: &Scratch, r: &Run) {
    let mut sources = String::new();
    for f in ["stacks/platform.df", "traefik.df"] {
        sources.push_str(&s.read(f));
    }
    for out in [&r.stdout, &r.stderr] {
        assert!(
            sources.contains("extern") || !out.contains("extern"),
            "a message names `extern`:\n{out}"
        );
    }
}

/// The loader in a used module's resource clause, as traefik.df vendors
/// Traefik's CRDs: a resource per document, under the module's name.
#[test]
fn a_loader_in_a_used_modules_clause() {
    let s = project(
        "loader-module",
        "use k8s\nuse traefik\n",
        "resource k8s.config_map \"${d.metadata.name}\" = d where {\n  \
         d in yaml(\"vendor/maps.yml\")\n}\n",
    );
    let r = s.run(&["plan", "platform"]);
    plain(&s, &r);
    let r = r.success();
    for want in [
        "  + k8s.config_map traefik.a  traefik.df:1\n      data.x = \"1\"\n",
        "  + k8s.config_map traefik.b  traefik.df:1\n      data.x = \"2\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n{}", r.stdout);
    }
}

/// The loader in a component, made twice, in a module's `let`, and in the
/// stack's own rule: one document, read at each.
#[test]
fn a_loader_in_a_component_a_copy_and_a_let() {
    let s = project(
        "loader-component",
        "use k8s\nuse traefik\n\
         resource traefik.maps one {}\nresource traefik.maps two {}\n\
         decl seen(n: string)\n\
         seen(n) where n = traefik.first\n\
         seen(n) where d in yaml(\"vendor/maps.yml\"), n = \"stack.${d.metadata.name}\"\n",
        "let first = yaml(\"vendor/maps.yml\")[0].metadata.name\n\n\
         component maps {\n  \
         resource k8s.config_map \"${d.metadata.name}\" = d where {\n    \
         d in yaml(\"vendor/maps.yml\")\n  }\n}\n",
    );
    let r = s.run(&["plan", "platform"]);
    plain(&s, &r);
    let r = r.success();
    for want in [
        "  + traefik.maps one\n    + k8s.config_map one.a",
        "    + k8s.config_map one.b",
        "  + traefik.maps two\n    + k8s.config_map two.a",
        "    + k8s.config_map two.b",
    ] {
        assert!(r.stdout.contains(want), "{want}\n{}", r.stdout);
    }
    let r = s
        .run(&["dev", "query", "seen(n)", "stacks/platform.df"])
        .success();
    assert_eq!(r.stdout, "N\n\"a\"\n\"stack.a\"\n\"stack.b\"\n");
}

/// A loader's mistakes in a module say what the program wrote: a source
/// of two arguments, a source reading its own rows.
#[test]
fn a_loaders_errors_in_a_module_name_no_compiler_word() {
    for (name, module, want) in [
        (
            "loader-args",
            "decl p(n: string)\np(n) where d in yaml(\"vendor/maps.yml\", \"x\"), \
             n = d.metadata.name\n",
            "yaml takes one source",
        ),
        (
            "loader-own-rows",
            "decl p(n: string)\np(n) where p(f), n = yaml(f).metadata.name\n",
            "yaml document: its source reads its own rows",
        ),
    ] {
        let s = project(name, "use k8s\nuse traefik\n", module);
        let r = s.run(&["plan", "platform"]);
        plain(&s, &r);
        let r = r.failure();
        assert!(r.stderr.contains(want), "{want}\n{}", r.stderr);
    }
}

/// A provider's `use` in a used module configures the provider for the
/// deployment, as the stack's would: with none in the stack, and beside
/// the stack's when they agree.
#[test]
fn a_provider_used_from_a_used_module() {
    let module = "use k8s { namespace = \"edge\" }\n\n\
                  resource k8s.config_map \"${d.metadata.name}\" = d where {\n  \
                  d in yaml(\"vendor/maps.yml\")\n}\n";
    for stack in [
        "use traefik\n",
        "use k8s { namespace = \"edge\" }\nuse traefik\n",
    ] {
        let s = project("provider-module", stack, module);
        let r = s.run(&["plan", "platform"]);
        plain(&s, &r);
        let r = r.success();
        assert_eq!(
            r.summary(),
            "plan: 2 changes (2 create) over 1 tick",
            "{}",
            r.stdout
        );
    }
}

/// Two `use`s that configure one provider differently are a conflict the
/// plan reports, naming the setting and both sites; it plans nothing.
#[test]
fn two_uses_that_disagree_are_a_conflict() {
    let s = project(
        "provider-conflict",
        "use k8s { namespace = \"core\" }\nuse traefik\n",
        "use k8s { namespace = \"edge\" }\n\n\
         resource k8s.config_map \"${d.metadata.name}\" = d where {\n  \
         d in yaml(\"vendor/maps.yml\")\n}\n",
    );
    let r = s.run(&["plan", "platform"]);
    plain(&s, &r);
    let r = r.failure();
    let want = "provider k8s: two configurations disagree at namespace: \
                stacks/platform.df:1:1 and traefik.df:1:1";
    assert!(r.stderr.contains(want), "{want}\n{}", r.stderr);
}

/// A provider is started by one source: two `use`s naming two are an
/// error naming both.
#[test]
fn two_uses_name_one_source() {
    let s = project(
        "provider-sources",
        "use fake { source = \"providers/a\" }\nuse traefik\n",
        "use fake { source = \"providers/b\" }\n",
    );
    let r = s.run(&["plan", "platform"]).failure();
    assert!(
        r.stderr
            .contains("traefik.df:1:1: provider fake: two `use`s name another source"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("the other `use`"), "{}", r.stderr);
}
