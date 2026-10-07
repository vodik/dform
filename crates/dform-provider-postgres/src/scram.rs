//! SCRAM-SHA-256 verifiers (RFC 5802, RFC 7677), as Postgres keeps a
//! password: `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`.
//! The provider computes one from the program's password and sends that
//! in `ALTER ROLE .. PASSWORD`, so the plaintext never reaches the
//! server's statement log, `pg_stat_activity`, or an error it echoes.
//! A verifier the server already keeps is checked against the password
//! (`matches`), so an update that does not change the password does not
//! write it again.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// The iterations a new verifier takes: the server's default
/// (`scram_iterations`).
pub const ITERATIONS: u32 = 4096;

/// The password as the server mixes it: SASLprep'd when it can be (valid
/// UTF-8 with no prohibited character), else its bytes as they are, as
/// libpq's `PQencryptPasswordConn` and the server do.
fn prepared(password: &str) -> Vec<u8> {
    match stringprep::saslprep(password) {
        Ok(p) => p.into_owned().into_bytes(),
        Err(_) => password.as_bytes().to_vec(),
    }
}

pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// `Hi(password, salt, i)`: PBKDF2-HMAC-SHA-256, one block.
pub fn salted(password: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let p = prepared(password);
    let mut block = salt.to_vec();
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac(&p, &block);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac(&p, &u);
        out.iter_mut().zip(u.iter()).for_each(|(o, x)| *o ^= x);
    }
    out
}

/// A salted password's StoredKey and ServerKey.
pub fn keys(salted: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let client = hmac(salted, b"Client Key");
    let stored: [u8; 32] = Sha256::digest(client).into();
    (stored, hmac(salted, b"Server Key"))
}

/// A verifier of `password` with `salt` and `iterations`.
pub fn verifier_with(password: &str, salt: &[u8], iterations: u32) -> String {
    let (stored, server) = keys(&salted(password, salt, iterations));
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        STANDARD.encode(salt),
        STANDARD.encode(stored),
        STANDARD.encode(server)
    )
}

/// A verifier of `password` with a fresh random salt.
pub fn verifier(password: &str) -> String {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("the system's random source answers");
    verifier_with(password, &salt, ITERATIONS)
}

/// A SCRAM verifier's parts: iterations, salt, StoredKey, ServerKey.
pub struct Parsed {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored: Vec<u8>,
    pub server: Vec<u8>,
}

pub fn parse(verifier: &str) -> Option<Parsed> {
    let rest = verifier.strip_prefix("SCRAM-SHA-256$")?;
    let (params, keys) = rest.split_once('$')?;
    let (iterations, salt) = params.split_once(':')?;
    let (stored, server) = keys.split_once(':')?;
    Some(Parsed {
        iterations: iterations.parse().ok()?,
        salt: STANDARD.decode(salt).ok()?,
        stored: STANDARD.decode(stored).ok()?,
        server: STANDARD.decode(server).ok()?,
    })
}

/// Whether `verifier` (as `pg_authid.rolpassword` keeps it) is of
/// `password`. Anything but a SCRAM verifier (an MD5 hash, none) is not.
pub fn matches(verifier: &str, password: &str) -> bool {
    let Some(v) = parse(verifier) else {
        return false;
    };
    let (stored, server) = keys(&salted(password, &v.salt, v.iterations));
    stored.as_slice() == v.stored && server.as_slice() == v.server
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7677's example: the user's password `pencil`, the salt and the
    /// iterations it gives, and the ServerSignature its keys make.
    #[test]
    fn the_keys_are_rfc_7677s() {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let (stored, server) = keys(&salted("pencil", &salt, 4096));
        let auth = "n=user,r=rOprNGfwEbeRWgbNEkqO,\
                    r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
                    s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,\
                    c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        assert_eq!(
            STANDARD.encode(hmac(&server, auth.as_bytes())),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
        let client_sig = hmac(&stored, auth.as_bytes());
        let proof: Vec<u8> = hmac(&salted("pencil", &salt, 4096), b"Client Key")
            .iter()
            .zip(client_sig.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        assert_eq!(
            STANDARD.encode(proof),
            "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
    }

    #[test]
    fn a_verifier_matches_its_password_only() {
        let v = verifier("hunter2");
        assert!(v.starts_with("SCRAM-SHA-256$4096:"), "{v}");
        assert!(!v.contains("hunter2"));
        assert!(matches(&v, "hunter2"));
        assert!(!matches(&v, "hunter3"));
        assert_ne!(v, verifier("hunter2"), "a fresh salt each time");
        assert!(!matches("md5abc", "hunter2"));
    }
}
