//! Provider tools: `dform provider check` and `dform provider schema`.

use super::{Outcome, launch};
use crate::partition;
use crate::plugin::{self, Providers};
use anyhow::{Result, bail};

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
            println!("{}", partition::fmt_atom(a));
        }
        Ok(Outcome::Done)
    }
}
