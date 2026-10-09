//! A value `--set` gives an input that its own check does not hold of is
//! refused before evaluation, as a value outside its type is: at the flag,
//! `--set agents=4: input agents is int check ..`, never a conflict of
//! the input's cell at its declaration's line (the README samples ticket).

use dform_core::ast::Stmt;
use dform_core::inputs::{Declared, leaves, set_facts};
use dform_core::value::Value;

/// The inputs `src` declares, as the stack's own, an object one by leaf.
fn declared(src: &str) -> Vec<Declared> {
    let program = dform_core::parser::parse_file("p.df", src).unwrap_or_else(|e| panic!("{e}"));
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Input(i) => Some(i),
            _ => None,
        })
        .flat_map(|i| match i.fields.is_empty() {
            true => vec![i.clone()],
            false => leaves(i),
        })
        .map(|i| {
            let name = i.name.clone();
            Declared::new("", i, Some(name), false)
        })
        .collect()
}

fn set(declared: &[Declared], k: &str, v: i64) -> Result<(), String> {
    set_facts(declared, &[(k.to_string(), Value::Int(v))])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[test]
fn a_set_value_its_check_refuses_is_an_error_at_the_flag() {
    let d = declared(
        "input agents: int = 2 check 0 <= agents <= 3\n\
         input db { backup_days: int = 3 check 1 <= backup_days <= 35 }\n",
    );
    assert_eq!(set(&d, "agents", 3), Ok(()));
    assert_eq!(
        set(&d, "agents", 4),
        Err("--set agents=4: input agents is int check 0 <= agents, agents <= 3".into())
    );
    let e = set(&d, "db.backup_days", 99).unwrap_err();
    assert!(
        e.starts_with("--set db.backup_days=99: input db.backup_days is int check "),
        "{e}"
    );
    assert_eq!(set(&d, "db.backup_days", 35), Ok(()));
    let whole = Value::Obj([("backup_days".to_string(), Value::Int(99))].into());
    let e = set_facts(&d, &[("db".to_string(), whole)])
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with("--set db: db.backup_days = 99 is not int check "),
        "{e}"
    );
}
