//! The parser's test suite (docs/grammar.md): every `.df` file in the
//! repository and `tests/syntax/ok/` parses and prints back byte for byte;
//! each `tests/syntax/err/*.df` fails with the diagnostics in its `.txt`.
//! Accept a changed `.txt` with `UPDATE_GOLDEN=1 cargo test --test syntax`.

use dform::syntax::parser::parse;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn df_files(dir: &Path, recurse: bool, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if recurse {
                df_files(&p, true, out);
            }
        } else if p.extension().is_some_and(|e| e == "df") {
            out.push(p);
        }
    }
}

/// Every `.df` file the repository ships, and the positive corpus.
fn corpus() -> Vec<PathBuf> {
    let root = repo();
    let mut out = Vec::new();
    df_files(&root, false, &mut out);
    for d in [
        "examples",
        "crates/dform-mock/schemas",
        "tests/fixtures",
        "tests/syntax/ok",
    ] {
        df_files(&root.join(d), true, &mut out);
    }
    out
}

fn rel(p: &Path) -> String {
    p.strip_prefix(repo()).unwrap().display().to_string()
}

#[test]
fn every_file_parses_and_prints_back() {
    let files = corpus();
    assert!(files.len() > 30, "{files:?}");
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        let parse = parse(&src);
        assert!(parse.errors.is_empty(), "{}: {:?}", rel(&f), parse.errors);
        assert_eq!(
            parse.syntax().to_string(),
            src,
            "{} is not lossless",
            rel(&f)
        );
    }
}

/// The files another file of the corpus imports: libraries (modules and
/// policy packs) that read the program's inputs by name, so they lower
/// inside the programs that import them, not on their own.
fn libraries(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(f).unwrap();
        for line in src.lines() {
            if let Some(rest) = line.trim().strip_prefix("import \"")
                && let Some(path) = rest.strip_suffix('"')
            {
                // A project's imports resolve from its root (`loader`).
                let lib = match dform::project::manifest_root(f) {
                    Some(root) if !path.starts_with("./") && !path.starts_with("../") => {
                        root.join(path)
                    }
                    _ => f.parent().unwrap().join(path),
                };
                out.push(std::fs::canonicalize(&lib).unwrap_or(lib));
            }
        }
    }
    out
}

/// The repository's programs lower to today's AST: the edition pragma is
/// there and nothing they use is pending (E §7's programs are parse-only).
/// A library is lowered through the programs that import it.
#[test]
fn every_program_lowers() {
    let files = corpus();
    let libraries = libraries(&files);
    for f in files {
        let name = rel(&f);
        if name.starts_with("tests/syntax/ok/e7") || libraries.contains(&f) {
            continue;
        }
        let program = dform::loader::load_program(std::slice::from_ref(&f))
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        if !name.contains("providers/") && !name.starts_with("crates/dform-mock/schemas/") {
            dform::transform::lower(&program).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }
}

/// What checking a file prints: its diagnostics, one per line group.
fn check(name: &str, src: &str) -> String {
    let err = match dform::parser::parse_file(name, src) {
        Ok(program) => match dform::transform::lower(&program) {
            Ok(_) => return "ok\n".to_string(),
            Err(e) => e,
        },
        Err(e) => e,
    };
    format!("{err:#}\n")
}

#[test]
fn every_error_file_reports_its_diagnostics() {
    let mut files = Vec::new();
    df_files(&repo().join("tests/syntax/err"), false, &mut files);
    assert!(!files.is_empty());
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut failed = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        let got = check(&rel(&f), &src);
        let want_path = f.with_extension("txt");
        if update {
            std::fs::write(&want_path, &got).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&want_path).unwrap_or_default();
        if got != want {
            failed.push(format!("{}:\n--- want\n{want}--- got\n{got}", rel(&f)));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// DESIGN.org acceptance: three independent syntax errors, three
/// diagnostics, each naming file:line:col and what was expected.
#[test]
fn three_independent_errors_are_three_diagnostics() {
    let name = "tests/syntax/err/three_errors.df";
    let src = std::fs::read_to_string(repo().join(name)).unwrap();
    let err = dform::parser::parse_file(name, &src).unwrap_err();
    let d = err.downcast_ref::<dform::diag::Diagnostics>().unwrap();
    assert_eq!(d.0.len(), 3, "{err}");
    let lines: Vec<String> = d.0.iter().map(|d| d.to_string()).collect();
    for (l, want) in lines.iter().zip([
        "three_errors.df:5:13: expected",
        "three_errors.df:7:11: expected",
        "three_errors.df:9:18: expected",
    ]) {
        assert!(l.contains(want), "{l}");
    }
    let rendered = d.render(false);
    assert!(rendered.contains("three_errors.df:5:13"), "{rendered}");
    assert!(rendered.ends_with("3 errors\n"), "{rendered}");
}
