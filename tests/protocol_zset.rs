//! The Z-set planner through the provider protocol: the fixture world
//! planned by the mock provider, as the CLI plans it (refresh facts feed
//! round 0, then the Z-set), on every backend: a process over gRPC, linked
//! in, linked in across prost.

mod common;

use dform::ast::{Atom, Program, Term};
use dform::plugin::{Config, Launch, Providers};
use dform::provider::ActionKind;
use dform::value::Value;
use dform::zset::Lifecycle;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn input(k: &str, v: &str) -> Atom {
    Atom {
        pred: "input".into(),
        args: vec![
            Term::Val(Value::Str(k.into())),
            Term::Val(Value::Str(v.into())),
        ],
        record: None,
        span: Default::default(),
    }
}

type Planned = Vec<(String, ActionKind, BTreeSet<String>)>;

/// Plan `program` against a world file and its state, the way the CLI
/// does, on every backend: the same plan on each.
fn plan(program: &Program, extra: &[Atom], world: &Path, state: &Path) -> Planned {
    // The process backend spawns the mock provider: in a test binary,
    // its own executable.
    let fake = common::exe("dform-provider-fake");
    let launches: [Box<dyn Launch>; 3] = [
        Box::new(dform_grpc::client::Process::Mock(fake.into())),
        Box::new(dform_mock::Linked::direct()),
        Box::new(dform_mock::Linked::wire()),
    ];
    let plans: Vec<Planned> = launches
        .iter()
        .map(|l| plan_on(&**l, program, extra, world, state))
        .collect();
    let spelled: Vec<String> = plans.iter().map(|p| format!("{p:?}")).collect();
    assert!(spelled.windows(2).all(|w| w[0] == w[1]), "{spelled:#?}");
    plans.into_iter().next().unwrap()
}

/// Refresh facts feed round 0, then the Z-set.
fn plan_on(
    launch: &dyn Launch,
    program: &Program,
    extra: &[Atom],
    world: &Path,
    state: &Path,
) -> Planned {
    let backend = Providers::start(
        launch,
        &[],
        &Config {
            world: world.to_path_buf(),
            inventory: world.with_extension("inv"),
            chaos: vec![],
            cache: None,
            ..Default::default()
        },
    )
    .unwrap();
    let schema = backend.schema().clone();
    let st = dform::state::State::load(state).unwrap();
    let mut extra = extra.to_vec();
    extra.extend(schema.facts.clone());
    extra.extend(backend.world_facts(&st).unwrap());
    // dform.df's settings are its stack config's table.
    let lowered = dform::transform::lower(program).unwrap();
    let tables = dform::tables::Tables::default();
    let externs = dform::externs::Externs::new(&lowered.program, &lowered.extern_fns, |f, ins| {
        tables.answer(f, ins).expect("only tables")
    });
    let (res, violations) = externs.eval(program, &extra).unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    let desired = dform::ir::compile_resources(res.facts.iter().cloned(), &schema).unwrap();
    backend
        .plan(&desired, &[], &Lifecycle::default(), &st)
        .unwrap()
        .actions
        .into_iter()
        .map(|a| (a.addr.to_string(), a.kind, a.on))
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dform-zset-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fixture(dir: &Path, edit: impl Fn(&mut serde_json::Value)) -> (PathBuf, PathBuf) {
    let mut w: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("tests/fixtures/world/dform.json")).unwrap(),
    )
    .unwrap();
    edit(&mut w);
    let world = dir.join("dform.json");
    std::fs::write(&world, serde_json::to_string(&w).unwrap()).unwrap();
    let state = dir.join("dform.state.json");
    std::fs::copy(root().join("tests/fixtures/world/dform.state.json"), &state).unwrap();
    (world, state)
}

fn count(p: &[(String, ActionKind, BTreeSet<String>)], k: fn(&ActionKind) -> bool) -> usize {
    p.iter().filter(|x| k(&x.1)).count()
}

/// F 4.3: on the fixture, env=prod is eleven updates (six of them
/// replacements, the fake schema's cidrs being force_new) and three
/// undeformed, the same eleven addresses the planner reported before the
/// Z-set replaced its diff; nothing is pending (round 0).
#[test]
fn fixture_prod_is_eleven_updates() {
    let dir = scratch("prod");
    let (world, state) = fixture(&dir, |_| {});
    let program =
        dform::loader::load_program(&[root().join("examples/demo/stacks/dform.df")]).unwrap();
    let p = plan(&program, &[input("env", "prod")], &world, &state);
    // Six of them change a force_new cidr: replacements.
    assert_eq!(count(&p, |k| matches!(k, ActionKind::Update)), 5);
    assert_eq!(count(&p, |k| matches!(k, ActionKind::Replace { .. })), 6);
    assert_eq!(count(&p, |k| matches!(k, ActionKind::Noop)), 3);
    assert_eq!(p.len(), 14);
    let noop: BTreeSet<&str> = p
        .iter()
        .filter(|x| matches!(x.1, ActionKind::Noop))
        .map(|x| x.0.as_str())
        .collect();
    assert_eq!(
        noop,
        BTreeSet::from([
            "iam.role[\"identity::app_role\"]",
            "iam.role_policy_attachment[\"identity::attach\"]",
            "net.vpc_peering[\"peer-main-peer\"]",
        ])
    );
    let p = plan(&program, &[], &world, &state);
    assert!(p.iter().all(|x| matches!(x.1, ActionKind::Noop)), "{p:?}");
}

/// F6: without the world's computed values round 0 resolves nothing, and
/// a fresh null against the world's constant is a stale identity: drift,
/// not ten spurious updates.
#[test]
fn a_fresh_null_against_a_world_constant_is_drift() {
    let dir = scratch("drift");
    let (world, state) = fixture(&dir, |w| {
        for r in w["resources"].as_object_mut().unwrap().values_mut() {
            r["computed"] = serde_json::json!({});
        }
    });
    let program =
        dform::loader::load_program(&[root().join("examples/demo/stacks/dform.df")]).unwrap();
    let p = plan(&program, &[], &world, &state);
    assert_eq!(count(&p, |k| matches!(k, ActionKind::Drift)), 10, "{p:?}");
    assert_eq!(count(&p, |k| matches!(k, ActionKind::Update)), 0, "{p:?}");
}

/// E §2.8: an update whose desired document carries an open null against
/// a world constant is pending on that null.
#[test]
fn an_open_null_against_a_world_constant_is_pending() {
    let dir = scratch("open");
    let world = dir.join("w.json");
    std::fs::write(
        &world,
        serde_json::json!({"resources": {"compute.vm::app": {
            "typ": "compute.vm", "name": "app",
            "attrs": {"db_host": "old.db.fake"}, "computed": {"id": "vm-1"}}}})
        .to_string(),
    )
    .unwrap();
    // The vm is dform's: state maps it.
    std::fs::write(
        dir.join("w.state.json"),
        serde_json::json!({"version": 1, "resources": {
            "compute.vm::app": {"provider": "fakecloud", "remote": "app"}}})
        .to_string(),
    )
    .unwrap();
    let program = dform::parser::parse_program(
        "resource db.postgres main { size = 1 }
         resource compute.vm app { db_host = ref(db.postgres, \"main\", \"endpoint\") }",
    )
    .unwrap();
    let p = plan(&program, &[], &world, &dir.join("w.state.json"));
    let app = p.iter().find(|x| x.0 == "compute.vm[\"app\"]").unwrap();
    assert!(matches!(app.1, ActionKind::Pending), "{p:?}");
    assert_eq!(
        app.2,
        BTreeSet::from(["db.postgres/main#endpoint".to_string()])
    );
}
