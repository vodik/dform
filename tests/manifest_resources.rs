//! A vendored manifest becomes resources (R-126): a loader over a `---`
//! stream is the list of its documents, and `resource T NAME = VALUE
//! where ..` makes a resource of each, its body the document.

mod common;
mod tables_common;
use tables_common::{push, repo, scratch};

/// Two CustomResourceDefinitions as Traefik ships them: one `---` stream,
/// an empty document between them.
const STREAM: &str = "\
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: middlewares.traefik.io
spec:
  group: traefik.io
  names:
    kind: Middleware
    plural: middlewares
  scope: Namespaced
---
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: tlsoptions.traefik.io
spec:
  group: traefik.io
  names:
    kind: TLSOption
    plural: tlsoptions
  scope: Namespaced
";

/// A `---` stream is the list of its documents (an empty one is none): a
/// clause walks it, a selector's `[*]` too, and a relation reads a row
/// per document, at the line it starts on. A file of one document is
/// that document, as it was.
#[test]
fn a_stream_of_documents_is_their_list() {
    let s = scratch("manifest-stream");
    s.write("crds.yml", STREAM);
    s.write("one.yml", "metadata:\n  name: single\n");
    s.write(
        "p.df",
        "\ninput crd from yaml(\"crds.yml\")\n\
         input named from yaml(\"crds.yml\")[*].metadata\n\
         decl crd(apiVersion: string, kind: string, metadata: any, spec: any)\n\
         decl named(name: string)\n\
         decl walked(n: string)\n\
         walked(n) where d in yaml(\"crds.yml\"), n = d.metadata.name\n\
         decl single(n: string)\nuse fake\n\
         single(n) where n = yaml(\"one.yml\").metadata.name\n",
    );
    let q = |goal: &str| s.run(&["dev", "query", goal, "p.df"]).success().stdout;
    assert_eq!(
        q("walked(n)"),
        "N\n\"middlewares.traefik.io\"\n\"tlsoptions.traefik.io\"\n"
    );
    assert_eq!(
        q("named(n)"),
        "N\n\"middlewares.traefik.io\"\n\"tlsoptions.traefik.io\"\n"
    );
    assert_eq!(q("single(n)"), "N\n\"single\"\n");
    let r = s
        .run(&[
            "why",
            "crd(_, _, {name: \"tlsoptions.traefik.io\"}, _)",
            "p.df",
        ])
        .success();
    assert!(r.stdout.contains("crds.yml:14"), "{}", r.stdout);
}

/// The stream read from a repository at a ref, as Traefik's tag holds it.
#[test]
fn a_stream_read_from_git_is_its_list() {
    let s = scratch("manifest-git");
    repo(&s, "traefik.git");
    push(&s, "crds.yml", STREAM, "v3.7.14");
    s.write(
        "p.df",
        "\nuse fake\ndecl walked(n: string)\n\
         walked(n) where d in yaml(git(\"traefik.git\", \"v3.7.14\", \"crds.yml\")), \
         n = d.metadata.name\n",
    );
    let r = s.run(&["dev", "query", "walked(n)", "p.df"]).success();
    assert_eq!(
        r.stdout,
        "N\n\"middlewares.traefik.io\"\n\"tlsoptions.traefik.io\"\n"
    );
}
