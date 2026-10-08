//! Approvals (README "Approvals"): policy says which deformations need one,
//! an approver signs the plan's digest, and apply verifies the signature
//! offline before its first Apply call.
//!
//! * `requires_approval(r, Reason)` is an ordinary relation the program
//!   derives over `deformation/3` in the policy pass; `r` is a resource
//!   reference (R-42), which plan, the plan file and the approver's check
//!   print as its address, `T["A"]` (H-16). No rows, no token needed.
//! * The plan digest is sha256 over the canonical JSON (sorted keys, no
//!   whitespace) of the plan file without its `digest` field: the delta,
//!   the inputs and the extern answers (a git table's commit among them) ([`crate::zset::file::PlanFile::digest`]).
//! * A token is a signed [`Statement`]: approver, digest, the stack and its
//!   key, expiry. Two shapes: a JWT (RS256, ES256 or EdDSA) whose claims
//!   are the statement's (`sub` for the approver, `exp` for the expiry),
//!   and a DSSE envelope (`payloadType` [`PAYLOAD_TYPE`]) whose payload is
//!   the statement, signed with ed25519. Either is verified against the
//!   stack's trust root: a JWKS document (`approvals = jwks("https://...")`
//!   or `jwks_file("path")`), the key found by `kid`.
//! * The approval service is not dform's: [`sign_dsse`] and [`sign_jwt`]
//!   are what the example signer (`dform-approve`) and the tests use.

use crate::spell;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The DSSE `payloadType` of an approval statement.
pub const PAYLOAD_TYPE: &str = "application/vnd.dform.approval+json";

/// How long a fetched JWKS document is used before it is fetched again.
pub const JWKS_MAX_AGE_SECS: u64 = 3600;

/// Where a stack's approvers' public keys are (`approvals = ...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jwks {
    /// `jwks("https://...")`: fetched at apply time when the cache beside
    /// the state is older than [`JWKS_MAX_AGE_SECS`].
    Url(String),
    /// `jwks_file("path")`, from the project root: offline.
    File(PathBuf),
}

/// One trust root: a JWKS, and the issuer a JWT from it must name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRoot {
    pub jwks: Jwks,
    pub issuer: Option<String>,
}

impl std::fmt::Display for TrustRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.jwks {
            Jwks::Url(u) => write!(f, "jwks(\"{u}\")"),
            Jwks::File(p) => write!(f, "jwks_file(\"{}\")", p.display()),
        }
    }
}

/// What an approver signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    /// The stack's name, without its key.
    pub stack: String,
    /// The deployment's key, `{env: "prod"}`; empty for an unkeyed stack.
    #[serde(default)]
    pub key: BTreeMap<String, String>,
    /// `sha256:...`, the plan's digest.
    pub digest: String,
    pub approver: String,
    /// RFC 3339, UTC (`2026-09-28T17:00:00Z`).
    pub expires: String,
}

/// A token that verified: the statement, how it was signed, and a digest
/// of the token's text (what the audit log keeps beside the statement).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verified {
    pub format: &'static str,
    pub keyid: String,
    pub statement: Statement,
    pub token: String,
}

/// What a token must say to approve this apply.
pub struct Expect<'a> {
    pub stack: &'a str,
    pub key: &'a [(String, String)],
    pub digest: &'a str,
    /// Seconds since the epoch.
    pub now: u64,
}

impl Expect<'_> {
    fn deployment(&self) -> String {
        deployment(
            self.stack,
            self.key.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        )
    }
}

fn deployment<'a>(stack: &str, key: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let key: Vec<String> = key.map(|(k, v)| format!("{k}={v}")).collect();
    if key.is_empty() {
        stack.to_string()
    } else {
        format!("{stack}[{}]", key.join(","))
    }
}

/// A token that verified but approves another plan: in controller mode an
/// old token for an earlier plan, not worth a log line.
#[derive(Debug)]
pub struct OtherDigest(pub String);

impl std::fmt::Display for OtherDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OtherDigest {}

/// A column of `requires_approval` or `approver_allowed` as text: a string
/// as itself, a reference as the plan prints its address (`T["A"]`).
fn text(t: &crate::ast::Term) -> String {
    use crate::value::Value;
    match t {
        crate::ast::Term::Val(Value::Str(s)) => s.clone(),
        crate::ast::Term::Val(Value::Ref { typ, name, attr }) => crate::ir::Address {
            typ: typ.clone(),
            name: name.clone(),
        }
        .attr(attr),
        crate::ast::Term::Val(v) => spell::value(v),
        t => format!("{t:?}"),
    }
}

/// The policy pass's `requires_approval(r, Reason)` rows, `r` as the plan
/// prints an address (`T["A"]`), sorted.
pub fn needs(facts: &std::collections::BTreeSet<crate::ast::Atom>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = facts
        .iter()
        .filter(|a| a.pred == "requires_approval" && a.args.len() == 2)
        .map(|a| (text(&a.args[0]), text(&a.args[1])))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Does the program state `approver_allowed(Who, r)` anywhere (a rule or a
/// fact)? When it does, an approver must satisfy it for every resource
/// that needs the approval.
pub fn restricts_approvers(program: &crate::ast::Program) -> bool {
    use crate::ast::Stmt;
    crate::modules::nested(&program.statements)
        .into_iter()
        .any(|s| match s {
            Stmt::Fact(a) => a.pred == "approver_allowed",
            Stmt::Rule(r) => r.head.pred == "approver_allowed",
            _ => false,
        })
}

/// Does `approver_allowed(who, d)` hold in `facts`? `d` is the address as
/// `needs` prints it; the row's resource is a reference or that text.
pub fn approver_allowed(
    facts: &std::collections::BTreeSet<crate::ast::Atom>,
    who: &str,
    d: &str,
) -> bool {
    use crate::ast::Term;
    use crate::value::Value;
    facts.iter().any(|a| {
        a.pred == "approver_allowed"
            && matches!(a.args.as_slice(),
                [Term::Val(Value::Str(w)), x] if w == who && text(x) == d)
    })
}

/// `v` as canonical JSON: object keys sorted, no whitespace.
pub fn canonical_json(v: &Json) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Json, out: &mut String) {
    match v {
        Json::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Json::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&m[k], out);
            }
            out.push('}');
        }
        Json::Array(xs) => {
            out.push('[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// sha256 of `bytes`, hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `sha256:` and the hex digest of `v`'s canonical JSON.
pub fn digest_of(v: &Json) -> String {
    format!("sha256:{}", sha256_hex(canonical_json(v).as_bytes()))
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `secs` as RFC 3339 in UTC, to the second.
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// `YYYY-MM-DDTHH:MM:SSZ` (or with `+00:00`) as seconds since the epoch.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once(['T', 't'])?;
    let time = time.split('.').next()?;
    let num = |x: &str| x.parse::<i64>().ok();
    let d: Vec<i64> = date.split('-').map(num).collect::<Option<_>>()?;
    let t: Vec<i64> = time.split(':').map(num).collect::<Option<_>>()?;
    let ([y, m, d], [hh, mm, ss]) = (d.as_slice(), t.as_slice()) else {
        return None;
    };
    if !(1..=12).contains(m) || !(1..=31).contains(d) || *hh > 23 || *mm > 59 || *ss > 60 {
        return None;
    }
    // days_from_civil.
    let y = if *m <= 2 { y - 1 } else { *y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if *m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// A stack's approvals trust roots, loaded: each key set, and the issuer
/// it is for.
pub type Roots = Vec<(jsonwebtoken::jwk::JwkSet, Option<String>)>;

/// The JWKS documents of `roots`, each with the issuer its JWTs must name.
/// A URL's document is cached in `cache_dir` and fetched again (with
/// `curl`) only when the cache is older than [`JWKS_MAX_AGE_SECS`]; a fetch
/// that fails falls back to a stale cache with a warning.
pub fn load_roots(roots: &[TrustRoot], cache_dir: &Path) -> Result<Roots> {
    let mut out = Vec::new();
    for r in roots {
        let text = match &r.jwks {
            Jwks::File(p) => std::fs::read_to_string(p)
                .with_context(|| format!("approvals {r}: read {}", p.display()))?,
            Jwks::Url(u) => cached_fetch(u, cache_dir).with_context(|| format!("approvals {r}"))?,
        };
        let set: jsonwebtoken::jwk::JwkSet = serde_json::from_str(&text)
            .with_context(|| format!("approvals {r}: not a JWKS document"))?;
        out.push((set, r.issuer.clone()));
    }
    Ok(out)
}

/// `url`'s body, through dform's HTTP client (R-103: no `curl`), within
/// [`JWKS_TIMEOUT`]; a status that is no success is an error naming it.
#[cfg(not(target_family = "wasm"))]
fn fetch(url: &str) -> Result<String> {
    let r = crate::http::send(
        crate::plugin::host::HttpRequest {
            method: "GET".into(),
            url: url.to_string(),
            timeout: Some(JWKS_TIMEOUT),
            ..Default::default()
        },
        None,
        None,
    )
    .map_err(|e| anyhow::anyhow!("fetch {}", e.message))?;
    if !(200..300).contains(&r.status) {
        bail!("fetch {url}: the server answered {}", r.status);
    }
    String::from_utf8(r.body).map_err(|_| anyhow::anyhow!("fetch {url}: not UTF-8 text"))
}

#[cfg(target_family = "wasm")]
fn fetch(url: &str) -> Result<String> {
    bail!("fetch {url}: no HTTP client in a wasm build of dform-core")
}

/// How long a JWKS fetch may take.
const JWKS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

fn cached_fetch(url: &str, cache_dir: &Path) -> Result<String> {
    let cache = cache_dir.join(format!("jwks-{}.json", &sha256_hex(url.as_bytes())[..16]));
    let age = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs());
    if let Some(age) = age
        && age < JWKS_MAX_AGE_SECS
    {
        return std::fs::read_to_string(&cache)
            .with_context(|| format!("read {}", cache.display()));
    }
    let fetched = fetch(url);
    match fetched {
        Ok(text) => {
            std::fs::create_dir_all(cache_dir)
                .with_context(|| format!("mkdir {}", cache_dir.display()))?;
            std::fs::write(&cache, &text).with_context(|| format!("write {}", cache.display()))?;
            Ok(text)
        }
        Err(e) if age.is_some() => {
            eprintln!("warning: {e:#}; using the cached JWKS {}", cache.display());
            std::fs::read_to_string(&cache).with_context(|| format!("read {}", cache.display()))
        }
        Err(e) => Err(e),
    }
}

/// Verify `token` (a JWT, a DSSE envelope, or the envelope in base64)
/// against `roots` and `expect`: its signature, then its digest, its stack
/// and key, and its expiry. The error names what failed.
pub fn verify(
    token: &str,
    roots: &[(jsonwebtoken::jwk::JwkSet, Option<String>)],
    expect: &Expect,
) -> Result<Verified> {
    let token = token.trim();
    let v = if token.starts_with('{') {
        verify_dsse(token, roots)?
    } else if token.split('.').count() == 3 && !token.contains(char::is_whitespace) {
        verify_jwt(token, roots)?
    } else {
        let decoded = STANDARD
            .decode(token)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .filter(|s| s.trim_start().starts_with('{'))
            .ok_or_else(|| {
                anyhow::anyhow!("approval: not a JWT or a DSSE envelope (nor one in base64)")
            })?;
        verify_dsse(&decoded, roots)?
    };
    let s = &v.statement;
    if s.digest != expect.digest {
        return Err(OtherDigest(format!(
            "approval by {}: it approves plan digest {}, and this plan's is {}",
            s.approver, s.digest, expect.digest
        ))
        .into());
    }
    let theirs = deployment(
        &s.stack,
        s.key.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    );
    if theirs != expect.deployment() {
        bail!(
            "approval by {}: it is for stack {theirs}, and this is {}",
            s.approver,
            expect.deployment()
        );
    }
    let Some(expires) = parse_rfc3339(&s.expires) else {
        bail!(
            "approval by {}: expires {:?} is not an RFC 3339 time",
            s.approver,
            s.expires
        );
    };
    if expires <= expect.now {
        bail!(
            "approval by {}: it expired at {} (now {})",
            s.approver,
            s.expires,
            rfc3339(expect.now)
        );
    }
    Ok(v)
}

/// The key `kid` names in `roots`.
fn find_key<'a>(
    roots: &'a [(jsonwebtoken::jwk::JwkSet, Option<String>)],
    kid: &str,
) -> Option<(&'a jsonwebtoken::jwk::Jwk, &'a Option<String>)> {
    roots
        .iter()
        .find_map(|(set, iss)| set.find(kid).map(|k| (k, iss)))
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "payloadType")]
    payload_type: String,
    payload: String,
    signatures: Vec<Signature>,
}

#[derive(Deserialize)]
struct Signature {
    #[serde(default)]
    keyid: String,
    sig: String,
}

/// DSSE's pre-authentication encoding.
fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {payload_type} {} ",
        payload_type.len(),
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

fn ed25519_key(jwk: &jsonwebtoken::jwk::Jwk) -> Option<ed25519_dalek::VerifyingKey> {
    use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve};
    let AlgorithmParameters::OctetKeyPair(p) = &jwk.algorithm else {
        return None;
    };
    if p.curve != EllipticCurve::Ed25519 {
        return None;
    }
    let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(&p.x).ok()?.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).ok()
}

fn verify_dsse(
    text: &str,
    roots: &[(jsonwebtoken::jwk::JwkSet, Option<String>)],
) -> Result<Verified> {
    let env: Envelope = serde_json::from_str(text)
        .context("approval: a DSSE envelope is {payloadType, payload, signatures}")?;
    if env.payload_type != PAYLOAD_TYPE {
        bail!(
            "approval: payloadType is {:?}; an approval's is {PAYLOAD_TYPE:?}",
            env.payload_type
        );
    }
    let payload = STANDARD
        .decode(&env.payload)
        .context("approval: the envelope's payload is not base64")?;
    let signed = pae(&env.payload_type, &payload);
    let mut tried = Vec::new();
    let mut keyid = None;
    for s in &env.signatures {
        let Some((jwk, _)) = find_key(roots, &s.keyid) else {
            tried.push(format!(
                "keyid {:?}: not in the stack's trust root",
                s.keyid
            ));
            continue;
        };
        let Some(key) = ed25519_key(jwk) else {
            tried.push(format!("keyid {:?}: not an Ed25519 key", s.keyid));
            continue;
        };
        let sig = STANDARD
            .decode(&s.sig)
            .ok()
            .and_then(|b| ed25519_dalek::Signature::from_slice(&b).ok());
        match sig.map(|sig| key.verify_strict(&signed, &sig)) {
            Some(Ok(())) => {
                keyid = Some(s.keyid.clone());
                break;
            }
            _ => tried.push(format!(
                "keyid {:?}: the signature does not verify",
                s.keyid
            )),
        }
    }
    let Some(keyid) = keyid else {
        if tried.is_empty() {
            bail!("approval: the envelope has no signature");
        }
        bail!("approval: signature: {}", tried.join("; "));
    };
    let statement: Statement = serde_json::from_slice(&payload).context(
        "approval: the payload is not a statement {stack, key, digest, approver, expires}",
    )?;
    Ok(Verified {
        format: "dsse",
        keyid,
        statement,
        token: text.to_string(),
    })
}

#[derive(Deserialize)]
struct Claims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    approver: Option<String>,
    digest: String,
    stack: String,
    #[serde(default)]
    key: BTreeMap<String, String>,
    exp: u64,
}

fn verify_jwt(
    token: &str,
    roots: &[(jsonwebtoken::jwk::JwkSet, Option<String>)],
) -> Result<Verified> {
    use jsonwebtoken::Algorithm;
    let header = jsonwebtoken::decode_header(token).context("approval: a JWT's header")?;
    if !matches!(
        header.alg,
        Algorithm::RS256 | Algorithm::ES256 | Algorithm::EdDSA
    ) {
        bail!(
            "approval: JWT algorithm {:?}; accepted are RS256, ES256 and EdDSA",
            header.alg
        );
    }
    let kid = header.kid.unwrap_or_default();
    let Some((jwk, issuer)) = find_key(roots, &kid) else {
        bail!("approval: signature: JWT kid {kid:?} is not in the stack's trust root");
    };
    let key = jsonwebtoken::DecodingKey::from_jwk(jwk)
        .with_context(|| format!("approval: the trust root's key {kid:?}"))?;
    let mut v = jsonwebtoken::Validation::new(header.alg);
    // Expiry is checked with the statement's, to say so.
    v.validate_exp = false;
    v.validate_aud = false;
    v.set_required_spec_claims(&["exp"]);
    if let Some(iss) = issuer {
        v.set_issuer(&[iss]);
        v.required_spec_claims.insert("iss".into());
    }
    let data = jsonwebtoken::decode::<Claims>(token, &key, &v).map_err(|e| {
        use jsonwebtoken::errors::ErrorKind;
        match e.kind() {
            ErrorKind::InvalidSignature => {
                anyhow::anyhow!("approval: signature: the JWT's signature does not verify")
            }
            ErrorKind::InvalidIssuer => anyhow::anyhow!(
                "approval: the JWT's issuer is not the trust root's ({})",
                issuer.as_deref().unwrap_or_default()
            ),
            _ => anyhow::anyhow!("approval: JWT: {e}"),
        }
    })?;
    let c = data.claims;
    let Some(approver) = c.approver.or(c.sub) else {
        bail!("approval: the JWT names no approver (`sub`)");
    };
    Ok(Verified {
        format: "jwt",
        keyid: kid,
        statement: Statement {
            stack: c.stack,
            key: c.key,
            digest: c.digest,
            approver,
            expires: rfc3339(c.exp),
        },
        token: token.to_string(),
    })
}

/// An Ed25519 key from its 32-byte seed file.
pub fn signing_key(seed: &Path) -> Result<ed25519_dalek::SigningKey> {
    let bytes = std::fs::read(seed).with_context(|| format!("read {}", seed.display()))?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("{}: not a 32-byte Ed25519 seed", seed.display()))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

/// The public JWK of `key`, named `kid`: what a trust root lists.
pub fn public_jwk(key: &ed25519_dalek::SigningKey, kid: &str) -> Json {
    serde_json::json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "alg": "EdDSA",
        "use": "sig",
        "kid": kid,
        "x": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
    })
}

/// `statement` in a DSSE envelope signed by `key` as `kid`.
pub fn sign_dsse(key: &ed25519_dalek::SigningKey, kid: &str, statement: &Statement) -> Json {
    use ed25519_dalek::Signer;
    let payload = canonical_json(&serde_json::to_value(statement).unwrap_or_default());
    let sig = key.sign(&pae(PAYLOAD_TYPE, payload.as_bytes()));
    serde_json::json!({
        "payloadType": PAYLOAD_TYPE,
        "payload": STANDARD.encode(payload.as_bytes()),
        "signatures": [{ "keyid": kid, "sig": STANDARD.encode(sig.to_bytes()) }],
    })
}

/// `statement` as an EdDSA JWT signed by `key` as `kid`: the approver is
/// `sub`, the expiry `exp`.
pub fn sign_jwt(
    key: &ed25519_dalek::SigningKey,
    kid: &str,
    statement: &Statement,
) -> Result<String> {
    use ed25519_dalek::Signer;
    let exp = parse_rfc3339(&statement.expires)
        .ok_or_else(|| anyhow::anyhow!("expires {:?}: not RFC 3339", statement.expires))?;
    let header = serde_json::json!({ "alg": "EdDSA", "typ": "JWT", "kid": kid });
    let claims = serde_json::json!({
        "sub": statement.approver,
        "digest": statement.digest,
        "stack": statement.stack,
        "key": statement.key,
        "exp": exp,
    });
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(canonical_json(&header)),
        URL_SAFE_NO_PAD.encode(canonical_json(&claims))
    );
    let sig = key.sign(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trips() {
        for secs in [0, 951_782_400, 1_790_000_000, 4_102_444_800] {
            assert_eq!(
                parse_rfc3339(&rfc3339(secs)),
                Some(secs),
                "{}",
                rfc3339(secs)
            );
        }
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(
            parse_rfc3339("2000-02-29T00:00:00+00:00"),
            Some(951_782_400)
        );
        assert_eq!(parse_rfc3339("2000-13-01T00:00:00Z"), None);
    }

    #[test]
    fn canonical_json_sorts_keys_without_whitespace() {
        let v = serde_json::json!({"b": [1, {"d": 2, "c": "x y"}], "a": null});
        assert_eq!(
            canonical_json(&v),
            r#"{"a":null,"b":[1,{"c":"x y","d":2}]}"#
        );
    }

    fn roots(key: &ed25519_dalek::SigningKey) -> Vec<(jsonwebtoken::jwk::JwkSet, Option<String>)> {
        let set = serde_json::json!({ "keys": [public_jwk(key, "k1")] });
        vec![(serde_json::from_value(set).unwrap(), None)]
    }

    #[test]
    fn both_shapes_verify_and_a_changed_byte_does_not() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let st = Statement {
            stack: "app".into(),
            key: [("env".to_string(), "prod".to_string())].into(),
            digest: "sha256:ab".into(),
            approver: "alice".into(),
            expires: rfc3339(2_000),
        };
        let expect = Expect {
            stack: "app",
            key: &[("env".into(), "prod".into())],
            digest: "sha256:ab",
            now: 1_000,
        };
        let dsse = sign_dsse(&key, "k1", &st).to_string();
        let jwt = sign_jwt(&key, "k1", &st).unwrap();
        let b64 = STANDARD.encode(&dsse);
        for t in [&dsse, &jwt, &b64] {
            let v = verify(t, &roots(&key), &expect).unwrap();
            assert_eq!(v.statement, st);
        }
        let mut env: Json = serde_json::from_str(&dsse).unwrap();
        let forged = Statement {
            approver: "mallory".into(),
            ..st.clone()
        };
        env["payload"] =
            Json::String(STANDARD.encode(canonical_json(&serde_json::to_value(&forged).unwrap())));
        let e = verify(&env.to_string(), &roots(&key), &expect).unwrap_err();
        assert!(e.to_string().contains("does not verify"), "{e}");
        let other = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let e = verify(&jwt, &roots(&other), &expect).unwrap_err();
        assert!(e.to_string().contains("does not verify"), "{e}");
    }

    /// A one-request HTTP server on the loopback answering `status` with
    /// `body`.
    fn serve_once(status: u16, body: &'static str) -> String {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let mut s = s;
            write!(
                s,
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        format!("http://{addr}/jwks.json")
    }

    /// A JWKS URL is fetched by dform's HTTP client (R-103: no `curl`) and
    /// kept in the cache; a status that is no success is an error naming
    /// it.
    #[test]
    fn a_jwks_url_is_fetched_and_cached() {
        let dir = std::env::temp_dir().join(format!("dform-jwks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = serve_once(200, r#"{"keys":[]}"#);
        assert_eq!(cached_fetch(&url, &dir).unwrap(), r#"{"keys":[]}"#);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let url = serve_once(404, "no");
        let e = cached_fetch(&url, &dir.join("other")).unwrap_err();
        assert!(e.to_string().contains("answered 404"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
