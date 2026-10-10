//! The parser's test suite (docs/grammar.md): every `.df` file in the
//! repository and `tests/syntax/ok/` parses and prints back byte for byte;
//! each `tests/syntax/err/*.df` fails with the diagnostics in its `.txt`,
//! as the terminal shows them (`diag::report`), and so does each `tests/syntax/err/NAME/`, a project whose
//! `stacks/main.df` is loaded with the modules its paths name.
//! Accept a changed `.txt` with `UPDATE_GOLDEN=1 cargo test --test syntax`.

mod common;
use common::{corpus, df_files, rel, repo};
use dform::syntax::parser::parse;
use std::path::{Path, PathBuf};

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

/// The corpus's modules and components (R-65): a project's `.df` that is
/// not a stack (nor a provider's schema), and outside every project a
/// file another one beside it names by a `use` or an `instance`. Each
/// reads its user's names, so it lowers inside the programs that name it,
/// not on its own.
fn libraries(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for f in files {
        let named = |g: &PathBuf| {
            let stem = f.file_stem().unwrap().to_string_lossy().to_string();
            g != f
                && g.parent() == f.parent()
                && std::fs::read_to_string(g).unwrap().lines().any(|l| {
                    let l = l.trim();
                    ["use ", "instance "].iter().any(|kw| {
                        l.strip_prefix(kw)
                            .and_then(|r| r.split_whitespace().next())
                            .is_some_and(|p| p == stem)
                    })
                })
        };
        let library = match dform::project::manifest_root(f) {
            Some(root) => {
                let manifest = root.join(dform::project::MANIFEST);
                let text = std::fs::read_to_string(&manifest).unwrap();
                let stacks = dform::project::Manifest::parse(&manifest, &text)
                    .unwrap()
                    .stacks;
                let file = std::fs::canonicalize(f).unwrap();
                !dform::project::is_stack(&root, &file, &stacks) && !rel(f).contains("providers/")
            }
            None => files.iter().any(named),
        };
        if library {
            out.push(f.clone());
        }
    }
    out
}

/// The repository's programs lower to today's AST: the edition pragma is
/// there and nothing they use is pending (E §7's programs are parse-only).
/// A module is lowered through the programs that name it.
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
    dform::diag::report(&err, false)
}

/// What loading a project's stack prints (an error case that is a
/// directory, `tests/syntax/err/NAME/`, a project): the program of its
/// `stacks/main.df`, with the modules its paths name.
fn check_project(dir: &Path) -> String {
    let err = match dform::loader::load_program(&[dir.join("stacks/main.df")]) {
        Ok(program) => match dform::transform::lower(&program) {
            Ok(_) => return "ok\n".to_string(),
            Err(e) => e,
        },
        Err(e) => e,
    };
    dform::diag::report(&err, false)
}

#[test]
fn every_error_file_reports_its_diagnostics() {
    let mut files = Vec::new();
    df_files(&repo().join("tests/syntax/err"), false, &mut files);
    let mut projects: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/syntax/err"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    projects.sort();
    files.extend(projects);
    assert!(!files.is_empty());
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut failed = Vec::new();
    for f in files {
        let got = match f.is_dir() {
            true => check_project(&f),
            false => check(&rel(&f), &std::fs::read_to_string(&f).unwrap()),
        };
        let want_path = match f.is_dir() {
            true => f.with_file_name(format!("{}.txt", f.file_name().unwrap().to_string_lossy())),
            false => f.with_extension("txt"),
        };
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
        "three_errors.df:3:16: expected",
        "three_errors.df:5:14: expected",
        "three_errors.df:7:18: expected",
    ]) {
        assert!(l.contains(want), "{l}");
    }
    let rendered = d.render(false);
    assert!(rendered.contains("three_errors.df:3  "), "{rendered}");
    assert!(rendered.ends_with("3 errors\n"), "{rendered}");
}

/// R-109: a help is the fix for its error, so one help string never
/// serves two kinds of error (an error's kind: its code, `E0301`, when
/// it has one, else its message with the names, strings and numbers
/// taken out). Read off every error file's diagnostics; a help no other
/// kind shares is computed from its site or its own.
#[test]
fn no_help_serves_two_kinds_of_error() {
    let mut files = Vec::new();
    df_files(&repo().join("tests/syntax/err"), false, &mut files);
    let mut kinds: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    let mut txts: Vec<PathBuf> = files.iter().map(|f| f.with_extension("txt")).collect();
    for e in std::fs::read_dir(repo().join("tests/syntax/err")).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            txts.push(
                p.with_file_name(format!("{}.txt", p.file_name().unwrap().to_string_lossy())),
            );
        }
    }
    for txt in txts {
        let mut kind: Option<String> = None;
        for l in std::fs::read_to_string(&txt).unwrap().lines() {
            if let Some(help) = l.strip_prefix("  help: ") {
                if let Some(k) = &kind {
                    kinds.entry(help.to_string()).or_default().insert(k.clone());
                }
            } else if !l.starts_with("  ") {
                // `error  message  [E0304]`
                kind = l.split_once("  ").map(|(_, m)| kind_of(m.trim()));
            }
        }
    }
    let shared: Vec<String> = kinds
        .iter()
        .filter(|(_, k)| k.len() > 1)
        .map(|(h, k)| format!("{h}\n  serves {k:?}"))
        .collect();
    assert!(shared.is_empty(), "{}", shared.join("\n"));
}

/// An error's kind: its code when it has one, else its message with what
/// varies by site (a quoted name, a string, a dotted name, a number)
/// taken out.
fn kind_of(message: &str) -> String {
    if let Some(code) = message
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|w| w.len() == 5 && w.starts_with('E') && w[1..].bytes().all(|b| b.is_ascii_digit()))
    {
        return code.to_string();
    }
    let mut out = String::new();
    let mut rest = message;
    while let Some(c) = rest.chars().next() {
        let close = match c {
            '`' => Some('`'),
            '"' => Some('"'),
            _ => None,
        };
        match close.and_then(|q| rest[1..].find(q).map(|i| i + 2)) {
            Some(end) => {
                out.push('_');
                rest = &rest[end..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out.split(' ')
        .map(
            |w| match w.contains('.') || w.chars().any(|c| c.is_ascii_digit()) {
                true => "_",
                false => w,
            },
        )
        .collect::<Vec<_>>()
        .join(" ")
}
