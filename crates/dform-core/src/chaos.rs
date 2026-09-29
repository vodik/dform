//! Failure and latency injection for the fake provider (`dform dev --chaos SPEC apply`),
//! and one knob of the executor's own, `stop-after`, which works whatever
//! the backend.
//!
//! Deterministic: nothing sleeps and nothing is random. Time is the world's
//! tick counter, which every apply advances by one, and for `read-lag` the
//! count of Reads, so that a retried Read can see the object.
//!
//! An address is written as `plan` prints it, `T["N"]` (`ir::parse_address`);
//! quote the spec for the shell.
//!
//! | SPEC                           | effect                                                  |
//! |--------------------------------+---------------------------------------------------------|
//! | `fail=T["N"]`                  | Apply of T["N"] fails before it reaches the world       |
//! | `timeout=T["N"]`               | Apply of T["N"] takes effect, then the call times out   |
//! | `read-lag=T["N"]:K`            | the first K Reads of T["N"] after its Create miss it    |
//! | `mutate=T["N"].PATH=JSON`      | once, after the first tick T["N"] exists at, the world  |
//! |                                | sets its PATH to JSON                                   |
//! | `latency=T["N"]:MS`            | Apply of T["N"] is recorded as taking MS (never slept)  |
//! | `crash=T["N"]`                 | the provider dies as it is called to Apply T["N"]: exit |
//! |                                | 137 as a process, gone from then on when linked in      |
//! | `stop-after=N`                 | dform stops as if killed once N Apply calls returned,   |
//! |                                | each persisted: nothing in flight is waited for         |
//! | `fresh-ids`                    | every Create mints new ids, as a real cloud does: a     |
//! |                                | replacement's id is not its predecessor's               |

use crate::ir::Address;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default)]
pub struct Chaos {
    pub fail: BTreeSet<Address>,
    pub timeout: BTreeSet<Address>,
    pub read_lag: BTreeMap<Address, u64>,
    pub mutate: Vec<(Address, String, serde_json::Value)>,
    pub latency: BTreeMap<Address, u64>,
    pub crash: BTreeSet<Address>,
    /// The executor stops once this many Apply calls have returned.
    pub stop_after: Option<usize>,
    /// Every Create salts its minted values with a serial the world keeps,
    /// so a destroy-first replacement under the same name gets a new id.
    pub fresh_ids: bool,
}

use crate::ir::parse_resource_address as parse_addr;

/// `T["N"]:X`: the last `:` splits, so a name with `::` in it survives.
fn addr_and(s: &str, what: &str) -> Result<(Address, String)> {
    let (a, x) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected T[\"N\"]:{what}, got '{s}'"))?;
    Ok((parse_addr(a)?, x.to_string()))
}

impl Chaos {
    pub fn parse(specs: &[String]) -> Result<Chaos> {
        let mut c = Chaos::default();
        for spec in specs {
            c.add(spec).with_context(|| format!("--chaos {spec}"))?;
        }
        Ok(c)
    }

    fn add(&mut self, spec: &str) -> Result<()> {
        if spec == "fresh-ids" {
            self.fresh_ids = true;
            return Ok(());
        }
        let (knob, arg) = spec.split_once('=').ok_or_else(|| {
            anyhow!(
                "expected KNOB=ARG (fail, timeout, crash, read-lag, mutate, latency, \
                 stop-after) or fresh-ids"
            )
        })?;
        match knob {
            "fail" => {
                self.fail.insert(parse_addr(arg)?);
            }
            "timeout" => {
                self.timeout.insert(parse_addr(arg)?);
            }
            "crash" => {
                self.crash.insert(parse_addr(arg)?);
            }
            "stop-after" => {
                let n: usize = arg.parse().context("a number of Apply calls")?;
                if n == 0 {
                    bail!("stop-after=N: N is at least 1");
                }
                self.stop_after = Some(n);
            }
            "read-lag" => {
                let (a, k) = addr_and(arg, "READS")?;
                self.read_lag.insert(a, k.parse().context("reads")?);
            }
            "latency" => {
                let (a, ms) = addr_and(arg, "MS")?;
                self.latency.insert(a, ms.parse().context("milliseconds")?);
            }
            "mutate" => {
                let bad = || anyhow!("expected mutate=T[\"N\"].PATH=JSON");
                let (lhs, json) = arg.split_once('=').ok_or_else(bad)?;
                let (a, path) = crate::ir::parse_address(lhs)?;
                let path = path.ok_or_else(bad)?;
                let v =
                    serde_json::from_str(json).with_context(|| format!("JSON value '{json}'"))?;
                self.mutate.push((a, path, v));
            }
            other => {
                bail!(
                    "unknown chaos knob '{other}' (fail, timeout, crash, read-lag, mutate, latency, \
                     stop-after, fresh-ids)"
                )
            }
        }
        Ok(())
    }

    pub fn addresses(&self) -> BTreeSet<&Address> {
        let mut out: BTreeSet<&Address> = BTreeSet::new();
        out.extend(&self.fail);
        out.extend(&self.timeout);
        out.extend(&self.crash);
        out.extend(self.read_lag.keys());
        out.extend(self.latency.keys());
        out.extend(self.mutate.iter().map(|m| &m.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_after_counts_apply_calls() {
        let c = Chaos::parse(&["stop-after=3".into()]).unwrap();
        assert_eq!(c.stop_after, Some(3));
        assert!(c.addresses().is_empty());
        assert!(Chaos::parse(&["stop-after=0".into()]).is_err());
        assert!(Chaos::parse(&["stop-after=x".into()]).is_err());
    }

    #[test]
    fn specs_parse_with_scoped_names() {
        let c = Chaos::parse(&[
            r#"fail=net.subnet["private-a"]"#.into(),
            r#"read-lag=net.vpc["network.main::vpc"]:3"#.into(),
            r#"mutate=net.vpc["network.main::vpc"].tags.env="prod""#.into(),
            r#"latency=net.vpc["v"]:250"#.into(),
            "fresh-ids".into(),
        ])
        .unwrap();
        assert!(c.fresh_ids);
        assert!(
            c.fail
                .contains(&parse_addr(r#"net.subnet["private-a"]"#).unwrap())
        );
        assert_eq!(
            c.read_lag
                .get(&parse_addr(r#"net.vpc["network.main::vpc"]"#).unwrap()),
            Some(&3)
        );
        assert_eq!(c.mutate[0].1, "tags.env");
        assert_eq!(c.mutate[0].2, serde_json::json!("prod"));
        assert_eq!(c.latency.values().next(), Some(&250));
        assert!(Chaos::parse(&[r#"explode=net.vpc["v"]"#.into()]).is_err());
        assert!(Chaos::parse(&["fail=net.vpc/v".into()]).is_err());
        assert!(Chaos::parse(&["fail=novpc".into()]).is_err());
    }
}
