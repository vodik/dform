//! Failure and latency injection for the fake provider (`apply --chaos SPEC`).
//!
//! Deterministic: nothing sleeps and nothing is random. Time is the world's
//! tick counter, which every apply advances by one.
//!
//! | SPEC                           | effect                                                  |
//! |--------------------------------+---------------------------------------------------------|
//! | `fail=T/N`                     | Apply of T/N fails before it reaches the world          |
//! | `timeout=T/N`                  | Apply of T/N takes effect, then the call times out      |
//! | `read-lag=T/N:K`               | Read returns nothing for K ticks after T/N is created   |
//! | `mutate=T/N:PATH=JSON`         | after the tick, the world sets T/N's PATH to JSON       |
//! | `latency=T/N:MS`               | Apply of T/N is recorded as taking MS (never slept)     |

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
}

/// `T/N`: the type has no slash, the name may (component scopes use `::`).
pub fn parse_addr(s: &str) -> Result<Address> {
    let (typ, name) = s
        .split_once('/')
        .filter(|(t, n)| !t.is_empty() && !n.is_empty())
        .ok_or_else(|| anyhow!("expected an address TYPE/NAME, got '{s}'"))?;
    Ok(Address {
        typ: typ.to_string(),
        name: name.to_string(),
    })
}

/// `T/N:X`: the last `:` splits, so a name with `::` in it survives.
fn addr_and(s: &str, what: &str) -> Result<(Address, String)> {
    let (a, x) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected TYPE/NAME:{what}, got '{s}'"))?;
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
        let (knob, arg) = spec.split_once('=').ok_or_else(|| {
            anyhow!("expected KNOB=ARG (fail, timeout, read-lag, mutate, latency)")
        })?;
        match knob {
            "fail" => {
                self.fail.insert(parse_addr(arg)?);
            }
            "timeout" => {
                self.timeout.insert(parse_addr(arg)?);
            }
            "read-lag" => {
                let (a, k) = addr_and(arg, "TICKS")?;
                self.read_lag.insert(a, k.parse().context("ticks")?);
            }
            "latency" => {
                let (a, ms) = addr_and(arg, "MS")?;
                self.latency.insert(a, ms.parse().context("milliseconds")?);
            }
            "mutate" => {
                let (lhs, json) = arg
                    .split_once('=')
                    .ok_or_else(|| anyhow!("expected mutate=TYPE/NAME:PATH=JSON"))?;
                let (a, path) = addr_and(lhs, "PATH")?;
                let v =
                    serde_json::from_str(json).with_context(|| format!("JSON value '{json}'"))?;
                self.mutate.push((a, path, v));
            }
            other => {
                bail!("unknown chaos knob '{other}' (fail, timeout, read-lag, mutate, latency)")
            }
        }
        Ok(())
    }

    pub fn addresses(&self) -> BTreeSet<&Address> {
        let mut out: BTreeSet<&Address> = BTreeSet::new();
        out.extend(&self.fail);
        out.extend(&self.timeout);
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
    fn specs_parse_with_scoped_names() {
        let c = Chaos::parse(&[
            "fail=net.subnet/private-a".into(),
            "read-lag=net.vpc/network.main::vpc:3".into(),
            r#"mutate=net.vpc/network.main::vpc:tags.env="prod""#.into(),
            "latency=net.vpc/v:250".into(),
        ])
        .unwrap();
        assert!(
            c.fail
                .contains(&parse_addr("net.subnet/private-a").unwrap())
        );
        assert_eq!(
            c.read_lag
                .get(&parse_addr("net.vpc/network.main::vpc").unwrap()),
            Some(&3)
        );
        assert_eq!(c.mutate[0].1, "tags.env");
        assert_eq!(c.mutate[0].2, serde_json::json!("prod"));
        assert_eq!(c.latency.values().next(), Some(&250));
        assert!(Chaos::parse(&["explode=net.vpc/v".into()]).is_err());
        assert!(Chaos::parse(&["fail=novpc".into()]).is_err());
    }
}
