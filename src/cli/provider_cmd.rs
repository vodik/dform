//! Provider tools: `dform provider list`, `dform provider check` and
//! `dform provider schema`.

use super::{Cli, Outcome, launch};

use crate::plugin::source::{self, Lookup, Source};
use crate::plugin::{self, Providers};
use crate::project::{Located, Project};
use crate::report::table::{Cell, Table};
use crate::spell;
use anyhow::{Result, bail};
use std::path::Path;

/// `dform provider list`: each provider dform.toml names and where it was
/// found; outside a project, each provider executable the lookup finds.
#[derive(Debug, Clone)]
pub(super) struct ProviderList;

impl ProviderList {
    pub(super) fn run(&self, cli: &Cli) -> Result<Outcome> {
        let lookup = Lookup::here();
        let mut table = Table::new(["provider", "from", "path"]);
        match Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))? {
            Some(project) => {
                for name in project.manifest.providers.keys() {
                    let (from, path) = row(name, project.manifest.provider_location(name, &lookup));
                    table.push([name.clone(), from, path].map(Cell::text).into());
                }
            }
            None => {
                for (name, (place, path)) in lookup.installed() {
                    let row = [name, place.to_string(), path.display().to_string()];
                    table.push(row.map(Cell::text).into());
                }
            }
        }
        print!("{}", table.render(&cli.table));
        Ok(Outcome::Done)
    }
}

/// Where the provider `name` comes from, and its executable or schema.
fn row(name: &str, located: Option<Located>) -> (String, String) {
    match located {
        Some(Located::Found(place, exe)) => (place.to_string(), exe.display().to_string()),
        Some(Located::Path(exe)) if exe.exists() => ("its path".into(), exe.display().to_string()),
        Some(Located::Path(exe)) => ("its path, missing".into(), exe.display().to_string()),
        Some(Located::Source(spec)) => match source::resolve(&spec) {
            Source::Plugin(exe) => ("its source".into(), exe.display().to_string()),
            Source::Mock(schema) => ("a schema the mock plays".into(), schema),
        },
        None if name == "fake" => ("dform itself".into(), "dform __provider fake".into()),
        None => ("not found".into(), source::executable(name)),
    }
}

/// `dform provider check PATH`: the conformance suite.
#[derive(Debug, Clone)]
pub(super) struct ProviderCheck {
    pub(super) path: String,
}

impl ProviderCheck {
    pub(super) fn run(&self) -> Result<Outcome> {
        let path = &self.path;
        let (lines, failed) = plugin::check::run(launch(), path)?;
        for l in &lines {
            println!("{l}");
        }
        if failed > 0 {
            bail!(
                "provider {path}: {failed} of {} checks deviate",
                lines.len()
            );
        }
        println!("provider {path}: conforms");
        Ok(Outcome::Done)
    }
}

/// `dform provider schema PROVIDER`: its schema facts.
#[derive(Debug, Clone)]
pub(super) struct ProviderSchema {
    pub(super) provider: String,
}

impl ProviderSchema {
    pub(super) fn run(&self) -> Result<Outcome> {
        let backend = Providers::start(
            launch(),
            std::slice::from_ref(&self.provider),
            &plugin::Config::default(),
        )?;
        for a in &backend.schema().facts {
            println!("{}", spell::atom(a));
        }
        Ok(Outcome::Done)
    }
}
