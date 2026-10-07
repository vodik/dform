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
        "\ninput crd from yaml.decode(io.read(\"crds.yml\"))\n\
         input named from yaml.decode(io.read(\"crds.yml\"))[*].metadata\n\
         decl crd(apiVersion: string, kind: string, metadata: any, spec: any)\n\
         decl named(name: string)\n\
         decl walked(n: string)\n\
         walked(n) where d in yaml.decode(io.read(\"crds.yml\")), n = d.metadata.name\n\
         decl single(n: string)\nuse fake\n\
         single(n) where n = yaml.decode(io.read(\"one.yml\")).metadata.name\n",
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

/// A CustomResourceDefinition made of a stream's document says the row it
/// is, the file and the line its document starts on, and its size, not
/// its content (R-131); `-v` lays out the content; `--json` and the plan
/// file keep every leaf.
#[test]
fn a_resource_made_of_a_document_says_its_row() {
    let s = scratch("manifest-rows");
    let versions = "  scope: Namespaced\n  versions:\n    - name: v1alpha1\n";
    s.write(
        "crds.yml",
        &STREAM.replace("  scope: Namespaced\n", versions),
    );
    s.write(
        "p.df",
        "\nuse k8s\n\
         resource k8s.custom_resource_definition \"${d.metadata.name}\" = d \
         where d in yaml.decode(io.read(\"crds.yml\"))\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout.contains(
            "  + k8s.custom_resource_definition \"middlewares.traefik.io\"  p.df:3\n      \
             = crds.yml:2  (256 B)\n  \
             + k8s.custom_resource_definition \"tlsoptions.traefik.io\"  p.df:3\n      \
             = crds.yml:16  (253 B)\n"
        ),
        "{}",
        r.stdout
    );
    let v = s.run(&["plan", "-v", "p.df"]).success();
    assert!(
        v.stdout.contains(
            "      kind = \"CustomResourceDefinition\"\n      \
             metadata.name = \"tlsoptions.traefik.io\"\n      spec = {\n        \
             group: \"traefik.io\",\n        names: { kind: \"TLSOption\", plural: \"tlsoptions\" },\n"
        ),
        "{}",
        v.stdout
    );
    let j = s.run(&["plan", "--json", "p.df"]).success();
    assert!(j.stdout.contains("\"spec.names.plural\""), "{}", j.stdout);
    s.run(&["plan", "--out", "plan.json", "p.df"]).success();
    assert!(s.read("plan.json").contains("tlsoptions"));
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
         walked(n) where d in yaml.decode(io.read(\"git+file:traefik.git/crds.yml?ref=v3.7.14\")), \
         n = d.metadata.name\n",
    );
    let r = s.run(&["dev", "query", "walked(n)", "p.df"]).success();
    assert_eq!(
        r.stdout,
        "N\n\"middlewares.traefik.io\"\n\"tlsoptions.traefik.io\"\n"
    );
}

/// ConfigMaps as a vendored manifest lists them.
const CONFIG_MAPS: &str = "\
metadata:
  name: settings
  namespace: apps
data:
  mode: fast
  size: small
---
metadata:
  name: flags.v2
  namespace: apps
data:
  beta: \"on\"
";

/// `resource T NAME = VALUE where ..`: a resource per document, the
/// document its body, one contribution at the root: the rest of the
/// program contributes over it as over a block (a baseline's `@default`,
/// an `@override`), and a name holding a dot is one segment (R-112).
/// Applied, the next plan has nothing to do.
#[test]
fn a_resource_per_document_of_a_manifest() {
    let s = scratch("manifest-body");
    s.write("cms.yml", CONFIG_MAPS);
    s.write(
        "p.df",
        "\nuse k8s\n\
         resource k8s.config_map \"${d.metadata.name}\" = d where d in yaml.decode(io.read(\"cms.yml\"))\n\
         set r.metadata.labels.owner = \"ops\" @default where r in k8s\n\
         set c.data.size = \"large\" @override where c in k8s.config_map\n",
    );
    // A document the program modifies (R-131): its row, then the leaves
    // other writes made.
    let r = s.run(&["plan", "p.df"]).success();
    for want in [
        "  + k8s.config_map \"flags.v2\"  p.df:3\n      = cms.yml:8  (72 B)\n      \
         data.size = \"large\"      p.df:5\n      \
         metadata.labels.owner = \"ops\"\n",
        "  + k8s.config_map settings    p.df:3\n      = cms.yml:1  (89 B)\n      \
         data.size = \"large\"      p.df:5\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n{}", r.stdout);
    }
    // `-v`: the document folded, as the source wrote it.
    let r = s.run(&["plan", "-v", "p.df"]).success();
    for want in [
        "  + k8s.config_map \"flags.v2\"  p.df:3  with d = cms.yml:8  (72 B)\n      \
         data.beta = \"on\"\n      \
         data.size = \"large\"      p.df:5\n      \
         metadata = { name: \"flags.v2\", namespace: \"apps\" }\n      \
         metadata.labels.owner = \"ops\"\n",
        "  + k8s.config_map settings    p.df:3  with d = cms.yml:1  (89 B)\n      \
         data.mode = \"fast\"\n      data.size = \"large\"      p.df:5\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n{}", r.stdout);
    }
    s.run(&["apply", "--yes", "p.df"]).success();
    let r = s.run(&["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

/// An object written out is the block of its entries, checked as a
/// block's are; a value that is no object is an error, where the text
/// says so and where the value is only known at run time.
#[test]
fn a_value_body_is_an_object_of_the_type() {
    let s = scratch("manifest-typed");
    s.write(
        "p.df",
        "\nuse k8s\n\
         resource k8s.persistent_volume_claim c = { metadata: { name: \"c\" }, \
         spec: { resources: { requests: { storage: \"lots\" } } } }\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "k8s.persistent_volume_claim[\"c\"].spec.resources.requests.storage is bytes: \
             `lots` is not a quantity"
        ),
        "{}",
        r.stderr
    );
    s.write("p.df", "\nuse k8s\nresource k8s.config_map c = [\"x\"]\n");
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("the body of a resource is a value of its type, an object: not `[\"x\"]`"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        "\nuse k8s\nlet docs = [\"x\"]\nresource k8s.config_map c = docs\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "the body of a resource is a value of its type, an object: not [\"x\"] (at p.df:4:"
        ),
        "{}",
        r.stderr
    );
}

/// `fmt` prints a value body back as written, a long one broken as any
/// term is.
#[test]
fn fmt_keeps_a_value_body() {
    let s = scratch("manifest-fmt");
    let text = "use k8s\n\nresource k8s.config_map \"${d.metadata.name}\" @default = d where d in yaml.decode(io.read(\"cms.yml\"))\n";
    s.write("p.df", text);
    s.run(&["fmt", "p.df"]).success();
    assert_eq!(s.read("p.df"), text);
}
