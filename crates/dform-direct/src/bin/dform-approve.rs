//! `dform-approve`: the example signer of README "Approvals". The approval
//! service is not dform's; this signs a plan digest with a local Ed25519
//! key so the flow runs end to end (examples/approvals/).
//!
//!   dform-approve keygen KEY              a new key (32 bytes, 0600); prints its JWKS
//!   dform-approve jwks KEY                the key's JWKS: the stack's trust root
//!   dform-approve sign KEY --digest sha256:... --stack NAME [--key K=V]...
//!                 --approver WHO [--expires RFC3339 | --ttl SECS]
//!                 [--format dsse | jwt | fact]
//!
//! `sign` prints a DSSE envelope (`dsse`, the default), an EdDSA JWT
//! (`jwt`), or the envelope as an `approval/1` fact for a controller's
//! input relation (`fact`: `approval("BASE64")`). The key id is the first
//! 16 hex digits of the public key's sha256.

use anyhow::{Context, Result, bail};
use dform_core::approval::{self, Statement};
use std::path::Path;
use std::process::ExitCode;

fn kid(key: &ed25519_dalek::SigningKey) -> String {
    approval::sha256_hex(key.verifying_key().as_bytes())[..16].to_string()
}

fn jwks(key: &ed25519_dalek::SigningKey) -> String {
    let set = serde_json::json!({ "keys": [approval::public_jwk(key, &kid(key))] });
    serde_json::to_string_pretty(&set).unwrap_or_default()
}

fn keygen(path: &Path) -> Result<()> {
    use std::io::{Read, Write};
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut seed))
        .context("read /dev/urandom")?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)
        .and_then(|mut f| f.write_all(&seed))
        .with_context(|| format!("write {}", path.display()))?;
    println!("{}", jwks(&ed25519_dalek::SigningKey::from_bytes(&seed)));
    Ok(())
}

fn sign(path: &Path, args: &[String]) -> Result<()> {
    let key = approval::signing_key(path)?;
    let (mut digest, mut stack, mut approver) = (None, None, None);
    let (mut expires, mut format) = (None, "dsse".to_string());
    let mut pairs = std::collections::BTreeMap::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{flag}: a value"))
        };
        match flag.as_str() {
            "--digest" => digest = Some(value()?),
            "--stack" => stack = Some(value()?),
            "--approver" => approver = Some(value()?),
            "--key" => {
                let kv = value()?;
                let Some((k, v)) = kv.split_once('=') else {
                    bail!("--key {kv}: K=V");
                };
                pairs.insert(k.to_string(), v.to_string());
            }
            "--expires" => {
                let e = value()?;
                if approval::parse_rfc3339(&e).is_none() {
                    bail!("--expires {e}: RFC 3339, UTC (2026-09-28T17:00:00Z)");
                }
                expires = Some(e);
            }
            "--ttl" => {
                let secs: u64 = value()?.parse().context("--ttl: seconds")?;
                expires = Some(approval::rfc3339(approval::now() + secs));
            }
            "--format" => format = value()?,
            other => bail!("sign: unknown argument {other}"),
        }
    }
    let need = |x: Option<String>, flag: &str| x.ok_or_else(|| anyhow::anyhow!("sign: {flag}"));
    let st = Statement {
        stack: need(stack, "--stack")?,
        key: pairs,
        digest: need(digest, "--digest")?,
        approver: need(approver, "--approver")?,
        expires: expires.unwrap_or_else(|| approval::rfc3339(approval::now() + 3600)),
    };
    let kid = kid(&key);
    match format.as_str() {
        "dsse" => println!("{}", approval::sign_dsse(&key, &kid, &st)),
        "jwt" => println!("{}", approval::sign_jwt(&key, &kid, &st)?),
        "fact" => {
            use base64::Engine;
            let env = approval::sign_dsse(&key, &kid, &st).to_string();
            println!(
                "approval(\"{}\")",
                base64::engine::general_purpose::STANDARD.encode(env)
            );
        }
        other => bail!("--format {other}: dsse, jwt or fact"),
    }
    Ok(())
}

fn run(args: &[String]) -> Result<()> {
    match args {
        [cmd, key] if cmd == "keygen" => keygen(Path::new(key)),
        [cmd, key] if cmd == "jwks" => {
            println!("{}", jwks(&approval::signing_key(Path::new(key))?));
            Ok(())
        }
        [cmd, key, rest @ ..] if cmd == "sign" => sign(Path::new(key), rest),
        _ => bail!(
            "usage: dform-approve keygen KEY | jwks KEY | sign KEY --digest D --stack S \
             [--key K=V]... --approver WHO [--expires T | --ttl SECS] [--format dsse|jwt|fact]"
        ),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dform-approve: {e:#}");
            ExitCode::FAILURE
        }
    }
}
