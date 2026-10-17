//! The host's calls for one provider ([`Services`]): `dform:host` served
//! with that provider's grants. Both transports serve it: the gRPC `Host`
//! service (`grpc`) and the component imports (`wasm`).

use crate::signed::{self, Source, Steps};
use crate::{git, http, ratelimit, ssh};
use dform_core::plugin::credentials::{self, Credential, Kind};
use dform_core::plugin::host::{
    Calls, Endpoint, Error, Failure, GitFile, Grants, Handle, HttpRequest, HttpResponse, Level,
    Opened, Run, Target,
};
use dform_core::plugin::signer::{Material, Signer};
use dform_core::plugin::timed;
use dform_core::timing::location;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// How long before it expires a credential is opened again for the next
/// request (an exec plugin's token: the plugin runs again), so no request
/// goes out with a token that lapses on its way.
const RENEW_BEFORE: Duration = Duration::from_secs(10);

/// One provider's host: its grants and clients, which its calls only
/// read, and the credentials it opened and the tunnels it forwarded, by
/// handle. Its calls run at once (R-142): the handles are locked only to
/// insert or look one up, never across a call's I/O.
pub struct Services {
    grants: Grants,
    opened: Mutex<Held>,
    /// Held while an expired credential is opened again: one renewal at
    /// a time, the others finding it renewed.
    renewing: Mutex<()>,
    /// The pace of its requests, a window per account and bucket
    /// (`ratelimit`).
    limiter: Arc<ratelimit::Limiter>,
    ssh: Box<dyn ssh::Ssh>,
    git: git::Git,
    /// The provider's signer, when it has one: what makes its `signed`
    /// credentials and signs its requests with them, run apart from it
    /// (`crate::signed`).
    signer: OnceLock<Arc<dyn Signer + Send + Sync>>,
}

/// What a provider's calls opened, by handle.
struct Held {
    credentials: BTreeMap<Handle, Arc<Credential>>,
    tunnels: BTreeMap<Handle, (Endpoint, SocketAddr)>,
    next: Handle,
}

impl Held {
    fn handle(&mut self) -> Handle {
        let h = self.next;
        self.next += 1;
        h
    }
}

impl Services {
    /// The host of the provider `grants` names.
    pub fn new(grants: Grants) -> Services {
        let limiter = Arc::new(ratelimit::Limiter::new(&grants.provider));
        Services::paced(grants, limiter)
    }

    /// The host of the provider `grants` names, its requests paced by
    /// `limiter`, which the provider's Configure answers tell each
    /// session's account (`ratelimit::Paced`).
    pub fn paced(grants: Grants, limiter: Arc<ratelimit::Limiter>) -> Services {
        Services {
            limiter,
            grants,
            opened: Mutex::new(Held {
                credentials: BTreeMap::new(),
                tunnels: BTreeMap::new(),
                next: 1,
            }),
            renewing: Mutex::new(()),
            ssh: ssh::client(),
            git: git::Git::cache(),
            signer: OnceLock::new(),
        }
    }

    /// The same, with `ssh` as its SSH client (tests).
    pub fn with_ssh(mut self, ssh: Box<dyn ssh::Ssh>) -> Services {
        self.ssh = ssh;
        self
    }

    /// The same, its provider's signer `signer` (a component's, or an
    /// in-process test's).
    pub fn with_signer(self, signer: Arc<dyn Signer + Send + Sync>) -> Services {
        self.set_signer(signer);
        self
    }

    /// Its provider's signer is `signer`, served by the provider's process
    /// once it is started (a native provider's): the first set holds.
    pub fn set_signer(&self, signer: Arc<dyn Signer + Send + Sync>) {
        let _ = self.signer.set(signer);
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    fn opened(&self) -> std::sync::MutexGuard<'_, Held> {
        // Each use is one insert or lookup: a panic leaves the maps whole.
        self.opened.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The credential and the tunnel's end the handles `auth` and `via`
    /// name.
    fn held(
        &self,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<(Option<Arc<Credential>>, Option<SocketAddr>), Error> {
        let cred = auth.map(|h| self.credential(h)).transpose()?;
        let o = self.opened();
        let via = match via {
            None => None,
            Some(h) => Some(
                o.tunnels
                    .get(&h)
                    .map(|(_, a)| *a)
                    .ok_or_else(|| Error::fatal(format!("no tunnel {h} is open")))?,
            ),
        };
        Ok((cred, via))
    }

    /// The credential opened as `h`, opened again first when it expires
    /// within [`RENEW_BEFORE`] (an exec plugin's token).
    fn credential(&self, h: Handle) -> Result<Arc<Credential>, Error> {
        let opened = || {
            self.opened()
                .credentials
                .get(&h)
                .cloned()
                .ok_or_else(|| Error::fatal(format!("no credential {h} is open")))
        };
        let renew_by = SystemTime::now() + RENEW_BEFORE;
        let c = opened()?;
        if !c.expired_by(renew_by) {
            return Ok(c);
        }
        // A panic while renewing leaves nothing half done: take the lock.
        let _one = self.renewing.lock().unwrap_or_else(|e| e.into_inner());
        let c = opened()?;
        if !c.expired_by(renew_by) {
            return Ok(c);
        }
        let renewed = Arc::new(match &c.material {
            Some(material) => self.signed(&c.name, Some(material))?,
            None => self.load(&c.name)?,
        });
        self.opened().credentials.insert(h, renewed.clone());
        Ok(renewed)
    }

    /// The credential `name` as the host applies it: held for this
    /// provider (a setting of its Configure, R-45), else the operator's as
    /// dform.toml grants it (a file, or the environment's). A held one's
    /// error is why it does not load: its name is one the engine made,
    /// which the user never wrote.
    fn load(&self, name: &str) -> Result<Credential, Error> {
        if let Ok((Kind::Signed, _)) = credentials::parse_name(name) {
            return self.signed(name, None);
        }
        if let Some(held) = credentials::held(name, &self.grants.provider) {
            return held.map_err(|e| Error::fatal(format!("{e:#}")));
        }
        self.grants.credential(name)?;
        credentials::load(name).map_err(|e| {
            Error::fatal(format!(
                "provider {}: the credential {name}: {e:#}",
                self.grants.provider
            ))
        })
    }

    /// `req`, once its bucket's window admits it, sent with `cred`
    /// through `via`: an `io.call` span (`http`), the child of the
    /// provider call it is sent for while that call is traced, else of
    /// the span the host's thread is in. Its wait for the window is a
    /// `ratelimit.wait` span under it.
    // `parent` before `target`: tracing-attributes 0.1 takes a `parent`
    // after it for a second one.
    #[tracing::instrument(
        parent = &parent(req.call),
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} {} {}", self.grants.provider, req.method, location(&req.url)),
            otel.kind = "client",
            kind = "http",
            provider = self.grants.provider.as_str(),
            addr = %location(&req.url),
            bucket = req.bucket.as_deref(),
            status = tracing::field::Empty,
        )
    )]
    fn request(
        &self,
        req: HttpRequest,
        cred: Option<&Credential>,
        via: Option<SocketAddr>,
    ) -> Result<HttpResponse, Error> {
        let admitted = self.limiter.acquire(req.session, req.bucket.as_deref());
        let x = http::exchange(req, cred, via);
        admitted.answered(&x);
        if let Ok(r) = &x.answer {
            tracing::Span::current().record("status", r.status);
        }
        x.answer
    }
}

impl Services {
    /// The `signed` credential `name`, as the provider's signer makes it
    /// of the operator's grant (`crate::signed`), or renews it from its
    /// last `material`.
    fn signed(&self, name: &str, material: Option<&Material>) -> Result<Credential, Error> {
        let provider = &self.grants.provider;
        let signer = self.signer.get().ok_or_else(|| {
            Error::fatal(format!(
                "provider {provider}: the credential {name} is signed, and the provider has no \
                 signer"
            ))
        })?;
        let source = Source::of(name, provider, || self.grants.credential(name))?;
        let steps = Steps {
            host: self,
            signer: signer.as_ref(),
            source,
            name,
        };
        let material = match material {
            Some(m) => steps.renewed(m)?,
            None => steps.material()?,
        };
        Ok(signed::applied(name, material))
    }

    /// `req` as it is sent with `cred`: signed by the provider's signer
    /// when the credential's recipe says so; as it is otherwise.
    fn signing(&self, req: HttpRequest, cred: &Credential) -> Result<HttpRequest, Error> {
        let Some(material) = cred.material.as_ref().filter(|m| m.signs()) else {
            return Ok(req);
        };
        let signer = self.signer.get().ok_or_else(|| {
            Error::fatal(format!(
                "the credential {} is signed, and no signer is",
                cred.name
            ))
        })?;
        signed::sign(signer.as_ref(), req, material, &cred.name)
    }
}

/// The span a request sent for the call numbered `call` is under: that
/// call's while it is in flight, else the current one. Held while the
/// request's span is made, so the call's cannot end meanwhile.
fn parent(call: Option<u64>) -> tracing::Span {
    call.and_then(timed::call_span)
        .unwrap_or_else(tracing::Span::current)
}

impl Calls for Services {
    /// The credential `name`, opened ([`Services::load`]).
    fn open(&self, name: &str) -> Result<Opened, Error> {
        let c = self.load(name)?;
        let (endpoint, namespace) = (c.endpoint.clone(), c.namespace.clone());
        let mut o = self.opened();
        let handle = o.handle();
        o.credentials.insert(handle, Arc::new(c));
        Ok(Opened {
            handle,
            endpoint,
            namespace,
        })
    }

    fn send(
        &self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, Error> {
        let (cred, via) = self.held(auth, via)?;
        let req = match &cred {
            Some(c) => self.signing(req, c)?,
            None => req,
        };
        self.request(req, cred.as_deref(), via)
    }

    fn io_read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        // The run's reader (its known hosts, mirrors, declared schemes),
        // or, outside a run, one of the host's own.
        let files = self.grants.files.get();
        files.read_for(&self.grants, location)
    }

    fn io_read_versioned(&self, location: &str) -> Result<dform_core::files::Document, Failure> {
        let files = self.grants.files.get();
        files.read_for_document(&self.grants, location)
    }

    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        self.ssh.exec(on, argv, stdin)
    }

    fn write(&self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error> {
        self.ssh.write(on, path, data, mode)
    }

    fn forward(&self, via: &Target, to: &Endpoint) -> Result<Handle, Error> {
        let local = self.ssh.forward(via, to)?;
        let mut o = self.opened();
        let h = o.handle();
        o.tunnels.insert(h, (to.clone(), local));
        Ok(h)
    }

    fn tunnel(&self, h: Handle) -> Option<Endpoint> {
        self.opened().tunnels.get(&h).map(|(e, _)| e.clone())
    }

    fn git_commit(
        &self,
        repo: &str,
        branch: &str,
        files: Vec<GitFile>,
        message: &str,
    ) -> Result<String, Error> {
        self.git.commit(repo, branch, files, message)
    }

    fn log(&self, level: Level, message: &str) {
        let level = match level {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warning",
            Level::Error => "error",
        };
        eprintln!("provider {}: {level}: {message}", self.grants.provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dform_core::http::fake::Server;
    use dform_core::plugin::credentials::Secret;
    use dform_core::plugin::signer::{Got, Need, Outcome, Recipe, Request};

    /// A kubeconfig held for the provider `p` whose exec plugin answers
    /// `t1`, `t2`, .. (one more each run, counted in `dir`), expiring at
    /// `expires`; the server is `url`.
    fn exec_kubeconfig(dir: &std::path::Path, url: &str, expires: &str) -> credentials::Holding {
        let script = "n=$(( $(cat \"$DIR/runs\" 2>/dev/null || echo 0) + 1 )); echo $n > \"$DIR/runs\"; \
                      printf '{\"status\": {\"token\": \"t%s\", \"expirationTimestamp\": \"%s\"}}' $n \"$EXP\"";
        let kc = serde_json::json!({
            "current-context": "c",
            "contexts": [{"name": "c", "context": {"cluster": "k", "user": "u"}}],
            "clusters": [{"name": "k", "cluster": {"server": url}}],
            "users": [{"name": "u", "user": {"exec": {
                "apiVersion": "client.authentication.k8s.io/v1",
                "command": "sh", "args": ["-c", script],
                "env": [{"name": "DIR", "value": dir.display().to_string()},
                        {"name": "EXP", "value": expires}]}}}]
        });
        let parts = [(
            "kubeconfig".to_string(),
            Secret::new(kc.to_string().into_bytes()),
        )];
        credentials::hold(Kind::Kubeconfig, "p", parts.into())
    }

    /// The tokens two requests carry, sent with the credential `held`
    /// opened once.
    fn two_requests(server: &Server, held: &credentials::Holding) -> Vec<String> {
        let host = Services::new(Grants::none("p"));
        let h = host.open(held.name()).unwrap().handle;
        (0..2)
            .map(|_| {
                let req = HttpRequest {
                    method: "GET".into(),
                    url: server.url("/"),
                    ..HttpRequest::default()
                };
                let r = host.send(req, Some(h), None).unwrap();
                String::from_utf8(r.body).unwrap()
            })
            .collect()
    }

    /// A signer that makes a bearer token of the value the grant gives
    /// (here a setting held for the provider), which expires at `expires`,
    /// for the server `endpoint`; and renews it as `renewed`.
    struct Tokens {
        expires: Option<u64>,
        endpoint: Option<String>,
    }

    impl Signer for Tokens {
        fn needs(&self) -> Result<Vec<Need>, String> {
            Ok(vec![Need::Given, Need::Env(vec!["HOME".into()])])
        }

        fn step(&self, got: &[Got]) -> Result<Outcome, String> {
            match got {
                [Got::Given(given), Got::Env(env)] if env.is_empty() && given.len() == 1 => {
                    let token = String::from_utf8(given[0].1.clone()).map_err(|e| e.to_string())?;
                    Ok(Outcome::Material(Material {
                        recipe: vec![Recipe::Bearer(token), Recipe::ServerName("k".into())],
                        expires: self.expires,
                        endpoint: self.endpoint.clone(),
                        namespace: Some("shop".into()),
                        ..Material::default()
                    }))
                }
                other => Err(format!("got {other:?}")),
            }
        }

        fn sign(&self, _: Request, _: &Material, _: u64) -> Result<Request, String> {
            Err("a bearer token signs nothing".into())
        }

        fn refresh(&self, m: &Material, now: u64) -> Result<Outcome, String> {
            Ok(Outcome::Material(Material {
                recipe: vec![
                    Recipe::Bearer("renewed".into()),
                    Recipe::ServerName("k".into()),
                ],
                expires: Some(now + 3600),
                ..m.clone()
            }))
        }
    }

    /// A credential its provider makes is what its provider's signer
    /// makes of the grant (here the settings held for it; no variable is
    /// given), applied as each of its recipes says, the server and
    /// namespace it names the provider's to read, and renewed by the
    /// signer before it expires.
    #[test]
    fn a_signed_credential_is_its_signers_recipe_renewed_before_it_expires() {
        let server = Server::http1();
        server.answer(|s| {
            (
                200,
                Vec::new(),
                s.header("authorization").unwrap_or("").into(),
            )
        });
        let sent = |expires: Option<u64>| {
            let parts = [("token".to_string(), Secret::new(b"t1".to_vec()))];
            let held = credentials::hold(Kind::Signed, "p", parts.into());
            let endpoint = Some(server.url(""));
            let signer = Tokens { expires, endpoint };
            let host = Services::new(Grants::none("p")).with_signer(Arc::new(signer));
            let opened = host.open(held.name()).unwrap();
            assert_eq!(opened.endpoint, Some(server.url("")));
            assert_eq!(opened.namespace.as_deref(), Some("shop"));
            let c = host.credential(opened.handle).unwrap();
            assert_eq!(c.server_name.as_deref(), Some("k"));
            (0..2)
                .map(|_| {
                    let req = HttpRequest {
                        method: "GET".into(),
                        url: server.url("/"),
                        ..HttpRequest::default()
                    };
                    let r = host.send(req, Some(opened.handle), None).unwrap();
                    String::from_utf8(r.body).unwrap()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(sent(None), ["Bearer t1", "Bearer t1"]);
        assert_eq!(sent(Some(0)), ["Bearer renewed", "Bearer renewed"]);
    }

    /// An exec plugin's token is used until it expires, then the plugin
    /// runs again for the next request; one that does not expire runs
    /// once.
    #[test]
    fn an_exec_plugins_token_is_renewed_when_it_expires() {
        let server = Server::http1();
        server.answer(|s| {
            (
                200,
                Vec::new(),
                s.header("authorization").unwrap_or("").into(),
            )
        });
        let dir = std::env::temp_dir().join(format!("dform-renew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let held = exec_kubeconfig(&dir, &server.url(""), "2000-01-01T00:00:00Z");
        assert_eq!(two_requests(&server, &held), ["Bearer t2", "Bearer t3"]);
        std::fs::remove_file(dir.join("runs")).unwrap();
        let held = exec_kubeconfig(&dir, &server.url(""), "2999-01-01T00:00:00Z");
        assert_eq!(two_requests(&server, &held), ["Bearer t1", "Bearer t1"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
