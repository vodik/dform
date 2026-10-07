//! R-143: what a project's dform.toml grants a provider reaches the
//! launcher with that project's run, not through a process-global map.
//! One process (the language server, holding several workspaces)
//! evaluates three projects that run the same provider executable: one
//! granting it `wasi:filesystem`, one a credential, one naming it with no
//! table at all. Each start gets its own project's grants, and the host
//! beside the provider holds the same (its link's `hosting`); the third
//! gets none, not what the others left behind.

mod common;
use common::Scratch;
use dform::plugin::Launch;
use dform::plugin::host::Grants;
use dform::plugin::link::Link;
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// The CLI's launcher, recording each plugin start: the grants it was
/// passed, and the grants its host was started with.
struct Recording {
    inner: dform_host::Launcher,
    seen: RefCell<Vec<(Grants, Option<Grants>)>>,
}

impl Launch for Recording {
    fn mock(&self) -> anyhow::Result<Link> {
        self.inner.mock()
    }

    fn plugin(&self, exe: &Path, grants: &Grants) -> anyhow::Result<Link> {
        let link = self.inner.plugin(exe, grants)?;
        let hosted = link.hosting.as_ref().map(|h| h.grants.clone());
        self.seen.borrow_mut().push((grants.clone(), hosted));
        Ok(link)
    }
}

/// A project in `s/name` whose dform.toml names the fake provider's
/// executable as `entry` (`{exe}` replaced), and its stack.
fn project(s: &Scratch, name: &str, entry: &str, exe: &str) -> PathBuf {
    s.write(
        &format!("{name}/dform.toml"),
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nfake = {}\n",
            entry.replace("{exe}", exe)
        ),
    );
    s.write(
        &format!("{name}/main.df"),
        "use fake\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    s.path(&format!("{name}/main.df"))
}

fn grants(allow: &[&str], credentials: &[&str]) -> Grants {
    Grants {
        provider: "fake".into(),
        allow: allow.iter().map(|s| s.to_string()).collect(),
        credentials: credentials.iter().map(|s| s.to_string()).collect(),
        ..Grants::default()
    }
}

#[test]
fn two_projects_in_one_process_each_start_their_provider_with_their_grants() {
    let s = Scratch::new("provider-grants");
    let exe = common::exe("dform-provider-fake");
    let files = project(
        &s,
        "files",
        "{ path = \"{exe}\", allow = [\"wasi:filesystem\"] }",
        &exe,
    );
    let creds = project(
        &s,
        "creds",
        "{ path = \"{exe}\", credentials = [\"bearer:b\"] }",
        &exe,
    );
    let bare = project(&s, "bare", "\"{exe}\"", &exe);
    let launch = Recording {
        inner: dform_host::Launcher::Cli,
        seen: RefCell::new(Vec::new()),
    };
    let read = |p: &Path| std::fs::read_to_string(p);
    // Each in turn, then the first again: the order a language server
    // with three workspaces evaluates them as they are edited.
    for file in [&files, &creds, &bare, &files] {
        let o = dform_lsp::analysis::evaluate(
            &dform_lsp::analysis::Target {
                file: file.clone(),
                keys: Vec::new(),
            },
            &launch,
            &read,
            env!("CARGO_PKG_VERSION"),
        );
        assert!(
            o.evaluated.is_some(),
            "{}: {:?}",
            file.display(),
            o.problems.iter().map(|p| &p.message).collect::<Vec<_>>()
        );
    }
    let none = Grants::none_for(Path::new(&exe));
    let want = [
        grants(&["wasi:filesystem"], &[]),
        grants(&[], &["bearer:b"]),
        none,
        grants(&["wasi:filesystem"], &[]),
    ];
    let seen = launch.seen.into_inner();
    assert_eq!(seen.len(), want.len(), "{seen:?}");
    for ((passed, hosted), want) in seen.into_iter().zip(want) {
        assert_eq!(passed, want);
        assert_eq!(hosted, Some(want));
    }
}
