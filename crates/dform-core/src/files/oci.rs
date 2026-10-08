//! `oci.resolve(ref)` (R-132): an image reference's tag pinned to the
//! digest its registry names now, through dform's HTTP client. A
//! registry v2 `HEAD /v2/REPOSITORY/manifests/TAG` answers it in
//! `Docker-Content-Digest` (an index's digest, so every platform's image
//! is pinned at once); a registry that asks for a token
//! (`WWW-Authenticate: Bearer realm=..,service=..,scope=..`) is given one
//! from its realm, anonymously (ghcr.io, docker.io, quay.io, codeberg.org
//! for a public image) or with the credential `[io] credentials` names for
//! `oci://REGISTRY/REPOSITORY` (a private one). A registry on the
//! loopback (`localhost:5000`) is spoken to in the clear, as Docker does.
//!
//! The answer is the reference with its tag kept and its digest set
//! (`ghcr.io/element-hq/synapse:v1.139.0@sha256:..`): the plan prints it
//! and the plan file records it as an extern's answer, so `apply PLAN`
//! applies what plan resolved; the next plan resolves again, and a tag
//! that moved is an update of what reads it. Each answer is kept in
//! `$XDG_CACHE_HOME/dform/oci/`, so a run that cannot reach the registry
//! (offline) takes the last digest it resolved; with none it is "not
//! yet", as a tag the registry does not have yet is.

use crate::ast::ExternFn;
use crate::plugin::host::{Class, Error, Failure, HttpRequest};
use crate::value::{OciRef, Value};
use anyhow::{Result, anyhow};
use std::path::PathBuf;

/// The built-in extern `oci.resolve(+reference: oci, -resolved: oci)`.
pub const RESOLVE: &str = "oci.resolve";

/// What a manifest is asked for as: an index (a multi-platform image)
/// first, then a single image's manifest, OCI's and Docker's.
const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

/// The answer to `oci.resolve`'s call; `None` for any other extern.
pub fn answer(
    f: &ExternFn,
    inputs: &[Value],
    files: &super::Files,
) -> Option<Result<Vec<Vec<Value>>>> {
    if f.name != RESOLVE {
        return None;
    }
    let text = match inputs {
        [Value::Oci(t)] | [Value::Str(t)] => t.clone(),
        _ => return Some(Err(anyhow!("oci.resolve takes an image reference"))),
    };
    let r = match OciRef::parse(&text) {
        Ok(r) => r,
        Err(why) => {
            return Some(Err(anyhow!(
                "oci.resolve: {text:?} is not an image reference {}: {why}",
                crate::value::OCI_GRAMMAR
            )));
        }
    };
    let row = |v: Value| vec![crate::externs::row(f, inputs, vec![v])];
    Some(match resolve(&r, files) {
        Ok(pinned) => Ok(row(pinned.value())),
        Err(Missing::NotYet(_)) => Ok(row(Value::Null {
            label: crate::value::null_label(RESOLVE, &text, "2"),
            class: crate::value::NullClass::Open,
            ty: String::new(),
        })),
        Err(Missing::Error(e)) => Err(anyhow!("oci.resolve({text:?}): {e}")),
    })
}

/// Why a reference has no digest now.
#[derive(Debug)]
pub enum Missing {
    /// The registry has no such tag yet, or cannot be reached and no
    /// digest was resolved before.
    NotYet(String),
    Error(String),
}

/// `r` pinned: its digest as the registry names its tag now, else (no
/// registry reached) as last resolved here. A reference that carries a
/// digest is pinned already.
pub fn resolve(r: &OciRef, files: &super::Files) -> Result<OciRef, Missing> {
    if r.digest.is_some() {
        return Ok(r.clone());
    }
    let mut r = r.clone();
    let tag = r.tag.get_or_insert_with(|| "latest".to_string()).clone();
    let key = r.to_string();
    #[cfg(not(target_family = "wasm"))]
    let asked = head(&r, &tag, files);
    #[cfg(target_family = "wasm")]
    let asked: Result<String, Missing> = {
        let _ = (files, &tag);
        Err(Missing::NotYet("no HTTP client in a wasm build".into()))
    };
    match asked {
        Ok(digest) => {
            remember(&key, &digest);
            r.digest = Some(digest);
            Ok(r)
        }
        Err(Missing::NotYet(why)) => match recalled(&key) {
            Some(d) => {
                r.digest = Some(d);
                Ok(r)
            }
            None => Err(Missing::NotYet(why)),
        },
        Err(e) => Err(e),
    }
}

/// The registry's host as its API is reached: Docker Hub's for the
/// default registry.
fn api_host(r: &OciRef) -> String {
    match r.registry.as_deref() {
        None => "registry-1.docker.io".to_string(),
        Some(h) => h.to_string(),
    }
}

/// Whether `host` (`localhost:5000`) is the machine's own, spoken to in
/// the clear.
fn loopback(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    matches!(name, "localhost" | "::1") || name.starts_with("127.")
}

/// `HEAD` of the tag's manifest: its digest.
#[cfg(not(target_family = "wasm"))]
fn head(r: &OciRef, tag: &str, files: &super::Files) -> Result<String, Missing> {
    let resp = manifest(r, tag, "HEAD", files)?;
    let registry = r.registry.as_deref().unwrap_or("docker.io");
    if let Some(d) = header(&resp.headers, "docker-content-digest") {
        return crate::value::OciRef::parse(&format!("x@{d}"))
            .map(|_| d.to_string())
            .map_err(|why| {
                Missing::Error(format!(
                    "{registry} named {}:{tag}'s digest {d:?}: {why}",
                    r.repository
                ))
            });
    }
    // A registry that names no digest on HEAD: the manifest's bytes are
    // what the digest is of.
    let body = manifest(r, tag, "GET", files)?;
    Ok(digest_of("sha256", &body.body).unwrap_or_default())
}

/// The manifest `reference` (a tag, or a digest) of `r`'s repository,
/// asked with `method`: the registry's answer once it is 200, a token
/// asked for first where it challenges for one.
#[cfg(not(target_family = "wasm"))]
fn manifest(
    r: &OciRef,
    reference: &str,
    method: &str,
    files: &super::Files,
) -> Result<crate::plugin::host::HttpResponse, Missing> {
    let host = api_host(r);
    let scheme = if loopback(&host) { "http" } else { "https" };
    let url = format!(
        "{scheme}://{host}/v2/{}/manifests/{reference}",
        r.repository
    );
    let shown = match reference.contains(':') {
        true => format!("{}@{reference}", r.repository),
        false => format!("{}:{reference}", r.repository),
    };
    let registry = r.registry.as_deref().unwrap_or("docker.io");
    let cred = credential(registry, &r.repository, files)?;
    let send = |method: &str,
                url: &str,
                auth: Option<&str>,
                cred: Option<&crate::plugin::credentials::Credential>| {
        let mut headers = vec![("Accept".to_string(), ACCEPT.to_string())];
        if let Some(t) = auth {
            headers.push(("Authorization".to_string(), format!("Bearer {t}")));
        }
        crate::http::send(
            HttpRequest {
                method: method.into(),
                url: url.to_string(),
                headers,
                timeout: Some(std::time::Duration::from_secs(30)),
                ..Default::default()
            },
            cred,
            None,
        )
        .map_err(|e| failed(registry, e))
    };
    let mut resp = send(method, &url, None, None)?;
    if resp.status == 401 {
        let challenge = header(&resp.headers, "www-authenticate").unwrap_or_default();
        match challenge.split_once(' ') {
            Some((s, params)) if s.eq_ignore_ascii_case("bearer") => {
                let t = token(registry, params, cred.as_ref(), &send)?;
                resp = send(method, &url, Some(&t), None)?;
            }
            // A registry that takes the credential itself.
            Some((s, _)) if s.eq_ignore_ascii_case("basic") && cred.is_some() => {
                resp = send(method, &url, None, cred.as_ref())?;
            }
            _ => {}
        }
    }
    match resp.status {
        200 => Ok(resp),
        401 | 403 => Err(Missing::Error(format!(
            "{registry} refused the pull of {} ({}): a private image's credential is named in \
             dform.toml, `[io] credentials = {{ \"oci://{registry}/{}\" = \"basic:NAME\" }}`",
            r.repository, resp.status, r.repository
        ))),
        404 => Err(Missing::NotYet(format!("{registry} has no {shown} yet"))),
        429 | 500..=599 => Err(Missing::NotYet(format!(
            "{registry} answered {} for {shown}",
            resp.status
        ))),
        s => Err(Missing::Error(format!(
            "{registry} answered {s} for the manifest of {shown}"
        ))),
    }
}

/// `ALGORITHM:HEX` of `bytes`, for a digest's algorithm dform computes
/// (`sha256`, `sha512`).
fn digest_of(algorithm: &str, bytes: &[u8]) -> Option<String> {
    use sha2::Digest;
    let hex = |sum: &[u8]| sum.iter().map(|b| format!("{b:02x}")).collect::<String>();
    match algorithm {
        "sha256" => Some(format!("sha256:{}", hex(&sha2::Sha256::digest(bytes)))),
        "sha512" => Some(format!("sha512:{}", hex(&sha2::Sha512::digest(bytes)))),
        _ => None,
    }
}

/// `io.read("oci://REGISTRY/REPOSITORY@sha256:..")` (R-153): the manifest
/// the digest names (an index's, or an image's), its bytes checked against
/// the digest and kept in the cache, so a run that cannot reach the
/// registry reads it too. A location names what it reads, so a tag is
/// not read: `oci.resolve` pins one.
pub fn read(u: &crate::uri::Uri, files: &super::Files) -> Result<Vec<u8>, Failure> {
    let port = u.port.map(|p| format!(":{p}")).unwrap_or_default();
    let text = format!("{}{port}{}", u.host.as_deref().unwrap_or_default(), u.path);
    let r = OciRef::parse(&text).map_err(|why| {
        Error::fatal(format!(
            "{u}: not an image reference {}: {why}",
            crate::value::OCI_GRAMMAR
        ))
    })?;
    let Some(digest) = r.digest.clone() else {
        return Err(Error::fatal(format!(
            "{u}: an `oci://` location names a manifest by its digest, `{u}@sha256:..`; a tag \
             is pinned to one by `oci.resolve`"
        ))
        .into());
    };
    let algorithm = digest.split(':').next().unwrap_or_default();
    if digest_of(algorithm, b"").is_none() {
        return Err(Error::fatal(format!(
            "{u}: dform checks a manifest by sha256 or sha512, not {algorithm}"
        ))
        .into());
    }
    let kept = cache().join("manifests").join(digest.replace(':', "-"));
    if let Ok(b) = std::fs::read(&kept)
        && digest_of(algorithm, &b).as_deref() == Some(digest.as_str())
    {
        return Ok(b);
    }
    #[cfg(not(target_family = "wasm"))]
    let asked = manifest(&r, &digest, "GET", files);
    #[cfg(target_family = "wasm")]
    let asked: Result<crate::plugin::host::HttpResponse, Missing> = {
        let _ = files;
        Err(Missing::NotYet("no HTTP client in a wasm build".into()))
    };
    let body = match asked {
        Ok(resp) => resp.body,
        Err(Missing::NotYet(why)) => return Err(Failure::NotYet(format!("{u}: {why}"))),
        Err(Missing::Error(e)) => return Err(Error::fatal(format!("{u}: {e}")).into()),
    };
    match digest_of(algorithm, &body) {
        Some(d) if d == digest => {
            if let Some(dir) = kept.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = crate::store::write_atomic(&kept, &body);
            Ok(body)
        }
        d => Err(Error::fatal(format!(
            "{u}: the registry answered a manifest whose digest is {}, not the one named",
            d.unwrap_or_default()
        ))
        .into()),
    }
}

/// The credential `[io] credentials` names for `oci://REGISTRY/REPO`.
#[cfg(not(target_family = "wasm"))]
fn credential(
    registry: &str,
    repository: &str,
    files: &super::Files,
) -> Result<Option<crate::plugin::credentials::Credential>, Missing> {
    let at = crate::uri::Uri::parse(&format!("oci://{registry}/{repository}"))
        .map_err(Missing::Error)?;
    match files.credential_for(&at) {
        None => Ok(None),
        Some(c) => crate::plugin::credentials::load(c).map(Some).map_err(|e| {
            Missing::Error(format!(
                "oci://{registry}/{repository}: the credential {c}: {e:#}"
            ))
        }),
    }
}

/// A request to the registry: method, URL, a token, the credential.
#[cfg(not(target_family = "wasm"))]
type Asks<'a> = dyn Fn(
        &str,
        &str,
        Option<&str>,
        Option<&crate::plugin::credentials::Credential>,
    ) -> Result<crate::plugin::host::HttpResponse, Missing>
    + 'a;

/// A token from the realm a `Bearer` challenge names (`realm="..",
/// service="..",scope=".."`), the credential applied when there is one.
#[cfg(not(target_family = "wasm"))]
fn token(
    registry: &str,
    params: &str,
    cred: Option<&crate::plugin::credentials::Credential>,
    send: &Asks<'_>,
) -> Result<String, Missing> {
    let p = challenge_params(params);
    let realm = p.get("realm").ok_or_else(|| {
        Missing::Error(format!(
            "{registry} asked for a token and named no realm to get one"
        ))
    })?;
    let mut q = Vec::new();
    for k in ["service", "scope"] {
        if let Some(v) = p.get(k) {
            q.push(format!(
                "{k}={}",
                percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
            ));
        }
    }
    let sep = if realm.contains('?') { '&' } else { '?' };
    let url = format!("{realm}{sep}{}", q.join("&"));
    let resp = send("GET", &url, None, cred)?;
    if resp.status != 200 {
        return Err(Missing::Error(format!(
            "{registry}'s token service {realm} refused ({}){}",
            resp.status,
            match cred {
                Some(c) => format!(" the credential {}", c.name),
                None => ": a private image's credential is named in dform.toml's [io] credentials"
                    .into(),
            }
        )));
    }
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap_or_default();
    body.get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| Missing::Error(format!("{registry}'s token service answered no token")))
}

/// `realm="https://ghcr.io/token",service="ghcr.io",scope="repository:o/r:pull"`.
fn challenge_params(s: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut rest = s.trim();
    while let Some((k, after)) = rest.split_once('=') {
        let k = k.trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let after = after.trim_start();
        let (v, next) = match after.strip_prefix('"') {
            Some(q) => match q.split_once('"') {
                Some((v, n)) => (v, n),
                None => (q, ""),
            },
            None => after.split_once(',').unwrap_or((after, "")),
        };
        out.insert(k, v.to_string());
        rest = next.trim_start_matches(',').trim_start();
    }
    out
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// A transport failure: one that reached nothing (offline, refused, a
/// name that does not resolve) is "not yet", the last digest standing in.
#[cfg(not(target_family = "wasm"))]
fn failed(registry: &str, e: Error) -> Missing {
    match e.class {
        Class::Retryable | Class::MaybeApplied => {
            Missing::NotYet(format!("{registry} cannot be reached: {}", e.message))
        }
        Class::Final => Missing::Error(e.message),
    }
}

/// Where resolved digests are kept: `$XDG_CACHE_HOME/dform/oci`.
fn cache() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("dform")
        .join("oci")
}

/// The file a reference's last digest is kept in: its text's digest.
fn kept(reference: &str) -> PathBuf {
    use sha2::Digest;
    let sum = sha2::Sha256::digest(reference.as_bytes());
    cache().join(sum.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn remember(reference: &str, digest: &str) {
    let path = kept(reference);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::store::write_atomic(&path, format!("{reference}\n{digest}\n").as_bytes());
}

fn recalled(reference: &str) -> Option<String> {
    let text = std::fs::read_to_string(kept(reference)).ok()?;
    let mut lines = text.lines();
    (lines.next()? == reference).then(|| lines.next().map(str::to_string))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_challenge_names_realm_service_and_scope() {
        let p = challenge_params(
            r#"realm="https://ghcr.io/token",service="ghcr.io",scope="repository:element-hq/synapse:pull""#,
        );
        assert_eq!(p["realm"], "https://ghcr.io/token");
        assert_eq!(p["service"], "ghcr.io");
        assert_eq!(p["scope"], "repository:element-hq/synapse:pull");
    }

    #[test]
    fn the_loopback_is_spoken_to_in_the_clear() {
        assert!(loopback("localhost:5000"));
        assert!(loopback("127.0.0.1:41234"));
        assert!(loopback("[::1]:5000"));
        assert!(!loopback("ghcr.io"));
        assert!(!loopback("registry.internal:5000"));
    }
}
