//! A wasm provider as one file: its component, built by cargo, made to
//! carry its signer's component and its `.df` sources as custom sections
//! (`dform::plugin::bundle`), so packaging, distributing and versioning a
//! provider is one artifact's.

use crate::Component;
use anyhow::{Context, Result};
use dform::plugin::bundle::{self, Sources};
use std::path::Path;

/// A wasm provider that carries more than its code.
pub struct Bundle {
    /// Its package.
    pub provider: &'static str,
    /// Its signer's package.
    pub signer: &'static str,
    /// The crate whose `src/` holds its `.df` files, from the repository.
    pub sources: &'static str,
}

/// The wasm providers with a signer.
pub const BUNDLES: [Bundle; 2] = [
    Bundle {
        provider: "dform-provider-ovh",
        signer: "dform-provider-ovh-signer",
        sources: "crates/dform-provider-ovh",
    },
    Bundle {
        provider: "dform-provider-k8s",
        signer: "dform-provider-k8s-signer",
        sources: "crates/dform-k8s",
    },
];

impl Bundle {
    /// Build its signer, then write it and its sources into its built
    /// provider's files under `target`, over what they carried: cargo's
    /// `dform_provider_ovh.wasm` and its hyphenated copy,
    /// `dform-provider-ovh.wasm`, which a dform.toml may name.
    pub fn build(&self, target: &Path) -> Result<()> {
        Component::of(self.signer).build()?;
        self.embed(target)
    }

    /// Its built signer and its sources written into its built provider,
    /// and into the hyphenated copy when cargo made one.
    fn embed(&self, target: &Path) -> Result<()> {
        let file = Component::of(self.provider).file(target);
        let copy = file.with_file_name(format!("{}.wasm", self.provider));
        let read = |p: &Path| std::fs::read(p).with_context(|| format!("read {}", p.display()));
        let signer = read(&Component::of(self.signer).file(target))?;
        let sources = self.sources()?.encode();
        let bundled = bundle::with_sections(
            &read(&file)?,
            &[(bundle::SIGNER, &signer), (bundle::SOURCES, &sources)],
        )
        .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
        for to in [&file, &copy] {
            if to == &file || to.exists() {
                std::fs::write(to, &bundled).with_context(|| format!("write {}", to.display()))?;
            }
        }
        Ok(())
    }

    /// Its `.df` files under its crate's `src/`, by path from the crate.
    fn sources(&self) -> Result<Sources> {
        let krate = crate::root().join(self.sources);
        let mut files = Vec::new();
        let mut dirs = vec![krate.join("src")];
        while let Some(dir) = dirs.pop() {
            for e in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
                let p = e?.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "df") {
                    let path = p.strip_prefix(&krate)?.to_string_lossy().into_owned();
                    files.push((path, std::fs::read(&p)?));
                }
            }
        }
        files.sort();
        Ok(Sources(files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A built provider is made to carry its built signer and its crate's
    /// `.df` files; made again, it is the same file; cargo's hyphenated
    /// copy of it is the same file too.
    #[test]
    fn a_provider_carries_its_signer_and_its_sources() {
        let target = crate::target_dir()
            .join("tmp")
            .join(format!("xtask-{}-bundle", std::process::id()));
        let _ = std::fs::remove_dir_all(&target);
        let b = &BUNDLES[0];
        let (provider, signer) = (
            Component::of(b.provider).file(&target),
            Component::of(b.signer).file(&target),
        );
        std::fs::create_dir_all(provider.parent().unwrap()).unwrap();
        std::fs::write(&provider, b"\0asm\x0d\x00\x01\x00").unwrap();
        let copy = provider.with_file_name(format!("{}.wasm", b.provider));
        std::fs::write(&copy, b"\0asm\x0d\x00\x01\x00").unwrap();
        std::fs::write(&signer, b"\0asm-the-signer").unwrap();
        b.embed(&target).unwrap();
        let bytes = std::fs::read(&provider).unwrap();
        assert_eq!(
            bundle::section(&bytes, bundle::SIGNER).unwrap(),
            Some(&b"\0asm-the-signer"[..])
        );
        let sources = bundle::section(&bytes, bundle::SOURCES).unwrap().unwrap();
        let sources = Sources::decode(sources).unwrap();
        let schema = std::fs::read(crate::root().join("crates/dform-provider-ovh/src/schema.df"));
        assert!(
            sources
                .0
                .contains(&("src/schema.df".into(), schema.unwrap()))
        );
        b.embed(&target).unwrap();
        assert_eq!(std::fs::read(&provider).unwrap(), bytes, "made again");
        assert_eq!(std::fs::read(&copy).unwrap(), bytes, "its copy");
        let _ = std::fs::remove_dir_all(&target);
    }
}
