//! `project::format_file`: what an editor's format request runs (the LSP's
//! `textDocument/formatting`) formats a project's file as `dform fmt`
//! does, with the project's schemas typing its literals.

mod common;
use common::Scratch;

#[test]
fn format_file_types_a_projects_literals_as_dform_fmt_does() {
    let src = "provider k8s\n\nresource k8s.deployment d {\n  \
               spec.template.spec.containers = [{ name: \"a\", resources: { limits: { memory: \"2Gi\" } } }]\n}\n";
    let p = Scratch::project("format-file");
    p.write("stacks/app.df", src);
    let path = p.path("stacks/app.df");
    let got = dform_core::project::format_file(&path, src, env!("CARGO_PKG_VERSION")).unwrap();
    p.run(&["fmt", "stacks/app.df"]).success();
    assert_eq!(got, p.read("stacks/app.df"));
    assert!(got.contains("memory: 2Gi"), "{got}");
    // Outside a project, no schema: the literal stays.
    let s = Scratch::new("format-file-none");
    s.write("app.df", src);
    let got = dform_core::project::format_file(&s.path("app.df"), src, "0.1.0").unwrap();
    assert!(got.contains("memory: \"2Gi\""), "{got}");
}
