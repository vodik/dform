use super::*;

/// `dform doc [TARGET]`: the doc comments of the project's .df files, or
/// of the target's program (its file and every file it imports), as
/// Markdown (`syntax::doc::markdown`), then the standard library's
/// functions (`syntax::doc::std_markdown`).
pub(super) fn doc(files: &[PathBuf]) -> Result<()> {
    let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let (title, files) = match (files, &project) {
        ([], Some(p)) => (p.manifest.project.name.clone(), crate::project::df_files(p)),
        ([], None) => return Err(crate::project::not_in_a_project(Path::new("."))),
        (fs, _) => (None, crate::loader::program_files(fs)?),
    };
    let title = title
        .or_else(|| {
            let f = files.first()?;
            Some(f.file_stem()?.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let mut trees = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?;
        let name = f.strip_prefix(&cwd).unwrap_or(f).display().to_string();
        let parse = crate::syntax::parser::parse(&text);
        if !parse.errors.is_empty() {
            return Err(crate::parser::syntax_diagnostics(&name, &text, &parse).into());
        }
        trees.push((name, parse.syntax()));
    }
    print!(
        "{}{}",
        crate::syntax::doc::markdown(&title, &trees),
        crate::syntax::doc::std_markdown()
    );
    Ok(())
}

/// `dform fmt`: rewrite each file in its formatted form, or with `check`
/// list the files that are not and fail.
pub(super) fn fmt_files(paths: &[PathBuf], check: bool) -> Result<()> {
    let mut unformatted = Vec::new();
    // Each project's typing (its providers' schemas, read offline), by root;
    // a file in no project has none.
    let mut typings: std::collections::BTreeMap<PathBuf, crate::fmt::Typing> =
        std::collections::BTreeMap::new();
    for p in paths {
        let src =
            std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("read {}: {e}", p.display()))?;
        let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
        let project =
            crate::project::Project::find(dir.unwrap_or(Path::new(".")), env!("CARGO_PKG_VERSION"))
                .ok()
                .flatten();
        let typing = project.map(|pr| {
            &*typings
                .entry(pr.root.clone())
                .or_insert_with(|| crate::fmt::Typing::of_project(&pr))
        });
        let out = crate::fmt::format_source_in(&p.display().to_string(), &src, typing)?;
        if out == src {
            continue;
        }
        if check {
            println!("{}", p.display());
            unformatted.push(p);
        } else {
            std::fs::write(p, out).map_err(|e| anyhow::anyhow!("write {}: {e}", p.display()))?;
        }
    }
    if !unformatted.is_empty() {
        bail!("{} file(s) not formatted", unformatted.len());
    }
    Ok(())
}
