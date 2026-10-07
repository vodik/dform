//! A zone's nameservers, asked of the machine's resolver over UDP (the
//! first `nameserver` of /etc/resolv.conf, or `DFORM_OVH_RESOLVER`,
//! `IP:PORT`): what a refusal names for a zone this account does not host.
//! Best effort: no answer within [`TIMEOUT`] is none.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

/// How long the resolver has to answer.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The NS records of `zone`, without their final dot; `None` when the
/// resolver gives none.
pub fn nameservers(zone: &str) -> Option<Vec<String>> {
    let server = resolver()?;
    let local: SocketAddr = match server {
        SocketAddr::V4(_) => "0.0.0.0:0".parse().ok()?,
        SocketAddr::V6(_) => "[::]:0".parse().ok()?,
    };
    let sock = UdpSocket::bind(local).ok()?;
    sock.set_read_timeout(Some(TIMEOUT)).ok()?;
    let id = (std::process::id() as u16) ^ 0x5a5a;
    sock.send_to(&query(id, zone)?, server).ok()?;
    let mut buf = [0u8; 4096];
    let (n, from) = sock.recv_from(&mut buf).ok()?;
    if from != server {
        return None;
    }
    let ns = answers(&buf[..n], id)?;
    (!ns.is_empty()).then_some(ns)
}

/// `DFORM_OVH_RESOLVER`, else the first `nameserver` of /etc/resolv.conf.
fn resolver() -> Option<SocketAddr> {
    if let Ok(a) = std::env::var("DFORM_OVH_RESOLVER") {
        return a.parse().ok();
    }
    let conf = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    conf.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next() == Some("nameserver"))
            .then(|| w.next()?.split('%').next()?.parse::<IpAddr>().ok())
            .flatten()
            .map(|ip| SocketAddr::new(ip, 53))
    })
}

/// A recursive query for `name`'s NS records.
fn query(id: u16, name: &str) -> Option<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    // Recursion desired; one question.
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    q.extend_from_slice(&encode(name)?);
    // QTYPE NS, QCLASS IN.
    q.extend_from_slice(&[0, 2, 0, 1]);
    Some(q)
}

/// `name` as DNS labels.
pub(crate) fn encode(name: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Some(out)
}

/// The NS names in the answer section of `msg`, the reply to `id`.
pub(crate) fn answers(msg: &[u8], id: u16) -> Option<Vec<String>> {
    let u16_at = |i: usize| Some(u16::from_be_bytes([*msg.get(i)?, *msg.get(i + 1)?]));
    if u16_at(0)? != id || msg.get(2)? & 0x80 == 0 || msg.get(3)? & 0x0f != 0 {
        return None;
    }
    let (qd, an) = (u16_at(4)?, u16_at(6)?);
    let mut at = 12;
    for _ in 0..qd {
        at = name(msg, at)?.1 + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        let after = name(msg, at)?.1;
        let (typ, len) = (u16_at(after)?, u16_at(after + 8)? as usize);
        let data = after + 10;
        if typ == 2 {
            out.push(name(msg, data)?.0);
        }
        at = data + len;
    }
    Some(out)
}

/// The name at `at` (its pointers followed) and where the record goes on.
pub(crate) fn name(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    for _ in 0..128 {
        let len = *msg.get(at)? as usize;
        match len {
            0 => return Some((labels.join("."), end.unwrap_or(at + 1))),
            l if l & 0xc0 == 0xc0 => {
                end.get_or_insert(at + 2);
                at = ((l & 0x3f) << 8) | *msg.get(at + 1)? as usize;
            }
            l => {
                let label = msg.get(at + 1..at + 1 + l)?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                at += 1 + l;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer as a resolver writes it: the question, then two NS
    /// records, the second's name a pointer into the first.
    #[test]
    fn reads_the_ns_records_of_an_answer() {
        let mut m = query(7, "vodik.xyz").unwrap();
        m[2] = 0x81;
        m[3] = 0x80;
        m[7] = 2;
        let first = m.len();
        for rdata in [
            encode("ns1.digitalocean.com").unwrap(),
            // "ns2" and a pointer to "digitalocean.com" in the first.
            vec![3, b'n', b's', b'2', 0xc0, (first + 12 + 4) as u8],
        ] {
            m.extend_from_slice(&[0xc0, 12, 0, 2, 0, 1, 0, 0, 1, 0]);
            m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            m.extend_from_slice(&rdata);
        }
        assert_eq!(
            answers(&m, 7).unwrap(),
            ["ns1.digitalocean.com", "ns2.digitalocean.com"]
        );
        assert_eq!(answers(&m, 8), None);
    }
}
