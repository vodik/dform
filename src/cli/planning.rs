use super::*;

/// Each change of `report` a run that does not hold the master plans,
/// marked (R-164): `secret changed, needs the key` when it would send a
/// stand-in, `secrets unchanged` when every secret leaf it derives was
/// proven unchanged (`.., a write-only one needs the key` when an update
/// would send one of those whole: the world does not answer it).
pub(super) fn custody_marks(
    report: &mut report::Report,
    plan: &crate::provider::Plan,
    backend: &crate::plugin::Providers,
) {
    if !crate::secrets::standin::active() {
        return;
    }
    for d in report.definite.iter_mut() {
        let Some(a) = plan.actions.iter().find(|a| a.addr == d.addr) else {
            continue;
        };
        let need = backend.needs_master(a, None);
        let proven = backend.proven(&a.addr);
        d.custody = match (need.is_empty(), proven.is_empty()) {
            // An update sends a write-only secret whole, unchanged or not.
            (false, _) if need.iter().all(|p| proven.contains(p)) => {
                Some("secrets unchanged, a write-only one needs the key".into())
            }
            (false, _) => Some("secret changed, needs the key".into()),
            // Unchanged in the program; the world's own value of one it
            // answers is compared only by a run with the key.
            (true, false) if !backend.answered(&a.addr, &proven).is_empty() => {
                Some("secrets unchanged, drift unknown without the key".into())
            }
            (true, false) => Some("secrets unchanged".into()),
            (true, true) => None,
        };
    }
}

/// What a run without the master cannot see (After R-164): a secret leaf
/// it proved unchanged in the program whose value the world answers (a
/// Secret's `stringData` key, not a write-only one) may have been changed
/// in the world by someone else; only a run with the key compares it. Said
/// on stderr, each leaf, so the plan never reads as "no drift".
pub(super) fn drift_unknown(
    deployment: &str,
    plan: &crate::provider::Plan,
    backend: &crate::plugin::Providers,
) {
    if !crate::secrets::standin::active() {
        return;
    }
    let leaves: Vec<String> = plan
        .actions
        .iter()
        .flat_map(|a| {
            backend
                .answered(&a.addr, &backend.proven(&a.addr))
                .into_iter()
                .map(|p| crate::report::attribute(&a.addr, &p))
        })
        .collect();
    if leaves.is_empty() {
        return;
    }
    eprintln!(
        "{deployment}: drift unknown without the key: {} the world holds {} compared with it only \
         by a run with the master: {}",
        match leaves.len() {
            1 => "1 secret".to_string(),
            n => format!("{n} secrets"),
        },
        if leaves.len() == 1 { "is" } else { "are" },
        leaves.join(", ")
    );
}

/// The bare diff's `needs approval:` section: each change and why.
pub(super) fn needs_text(needs: &[zset::file::NeedsApproval]) -> String {
    if needs.is_empty() {
        return String::new();
    }
    let mut out = String::from("needs approval:\n");
    for n in needs {
        out.push_str(&format!("  {}  ({})\n", n.deformation, n.reason));
    }
    out
}

/// A destroy's objects no Delete can reach ([`Planned::unreachable`]),
/// as the plan says a change and its reason.
pub(super) fn unreachable_text(unreachable: &[(ir::Address, String)]) -> String {
    if unreachable.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nunreachable  stay in state\n");
    for (a, why) in unreachable {
        out.push_str(&format!("  {}\n      {why}\n", report::address(a)));
    }
    out
}

/// The project's root, else the program's directory: what a site's place
/// is relative to.
pub(super) fn site_root(files: &[PathBuf]) -> Option<PathBuf> {
    files.first().and_then(|f| {
        let f = std::path::absolute(f).ok()?;
        crate::project::manifest_root(&f).or_else(|| f.parent().map(Path::to_path_buf))
    })
}
