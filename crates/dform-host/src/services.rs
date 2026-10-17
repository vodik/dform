//! The host's calls for one provider ([`Services`]): `dform:host` served
//! with that provider's grants. Both transports serve it: the gRPC `Host`
//! service (`grpc`) and the component imports (`wasm`).

use crate::signed::{self, Source, Steps};
use crate::{git, http, ratelimit, ssh};
use dform_core::plugin::credentials::{self, Credential};
use dform_core::plugin::host::{
    Calls, Endpoint, Error, Failure, GitFile, Grants, Handle, HttpRequest, HttpResponse, Level,
    Opened, Run, Target,
};
use dform_core::plugin::signer::{Material, Signer};
use dform_core::plugin::timed;
use dform_core::timing::{dry_run, location};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// How long before it expires a credential is made again for the next
/// request (a kubeconfig's exec plugin's token: its signer asks the
/// plugin run again), so no request goes out with a token that lapses on
/// its way.
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

    /// The credential opened as `h`, made again first when it expires
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
        if let Ok((kind, _)) = credentials::parse_name(name)
            && kind.made_by_signer()
        {
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
    /// through `via`: an `io.call` span (`http`), named by its method
    /// (its location a field, and a dry run said), the child of the
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
            otel.name = %format_args!(
                "{} {}{}",
                self.grants.provider,
                req.method,
                if dry_run(&req.url) { " (dry run)" } else { "" }
            ),
            otel.kind = "client",
            kind = "http",
            provider = self.grants.provider.as_str(),
            addr = %location(&req.url),
            dry_run = dry_run(&req.url).then_some(true),
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
    /// The credential `name` of a kind its provider's signer makes
    /// (`signed`, `kubeconfig`), as the signer makes it of the operator's
    /// grant (`crate::signed`), or renews it from its last `material`.
    fn signed(&self, name: &str, material: Option<&Material>) -> Result<Credential, Error> {
        let provider = &self.grants.provider;
        let source = Source::of(name, provider, || self.grants.credential(name))?;
        let signer = self.signer.get().ok_or_else(|| {
            Error::fatal(format!(
                "provider {provider}: the credential {name} is its signer's to make, and the \
                 provider has no signer"
            ))
        })?;
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

/// Where on `on` a span says an SSH call goes: `ssh://HOST[:PORT]PATH`,
/// without the user.
fn ssh_location(on: &Target, path: &str) -> String {
    match on.port {
        Some(port) => format!("ssh://{}:{port}{path}", on.host),
        None => format!("ssh://{}{path}", on.host),
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

    /// A read the provider asks of the host: an `io.call` span
    /// (`io.read`), as the program's own `io.read` is.
    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} io.read", self.grants.provider),
            otel.kind = "client",
            kind = "io.read",
            provider = self.grants.provider.as_str(),
            addr = %dform_core::timing::location(location),
        )
    )]
    fn io_read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        // The run's reader (its known hosts, mirrors, declared schemes),
        // or, outside a run, one of the host's own.
        let files = self.grants.files.get();
        files.read_for(&self.grants, location)
    }

    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} io.read", self.grants.provider),
            otel.kind = "client",
            kind = "io.read",
            provider = self.grants.provider.as_str(),
            addr = %dform_core::timing::location(location),
        )
    )]
    fn io_read_versioned(&self, location: &str) -> Result<dform_core::files::Document, Failure> {
        let files = self.grants.files.get();
        files.read_for_document(&self.grants, location)
    }

    /// A command run on `on` over SSH: an `io.call` span (`ssh.exec`),
    /// its exit status the answer's; so are a file written there, a
    /// tunnel forwarded through it, and a commit pushed to a repository.
    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} ssh exec", self.grants.provider),
            otel.kind = "client",
            kind = "ssh.exec",
            provider = self.grants.provider.as_str(),
            addr = %ssh_location(on, ""),
            status = tracing::field::Empty,
        )
    )]
    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        let run = self.ssh.exec(on, argv, stdin)?;
        tracing::Span::current().record("status", run.status);
        Ok(run)
    }

    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} ssh write", self.grants.provider),
            otel.kind = "client",
            kind = "ssh.write",
            provider = self.grants.provider.as_str(),
            addr = %ssh_location(on, path),
        )
    )]
    fn write(&self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error> {
        self.ssh.write(on, path, data, mode)
    }

    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} ssh forward", self.grants.provider),
            otel.kind = "client",
            kind = "ssh.forward",
            provider = self.grants.provider.as_str(),
            addr = %ssh_location(via, ""),
        )
    )]
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

    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} git commit", self.grants.provider),
            otel.kind = "client",
            kind = "git.commit",
            provider = self.grants.provider.as_str(),
            addr = %location(repo),
        )
    )]
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
    use dform_core::plugin::credentials::{Kind, Secret};
    use dform_core::plugin::signer::{Command, Got, Need, Outcome, Recipe, Request};

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

    /// The spans begun: each one's name and its fields, as text.
    #[derive(Clone, Default)]
    struct Begun(Arc<Mutex<Vec<(String, BTreeMap<String, String>)>>>);

    struct Fields<'a>(&'a mut BTreeMap<String, String>);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
            self.0.insert(f.name().to_string(), format!("{v:?}"));
        }

        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            self.0.insert(f.name().to_string(), v.to_string());
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Begun {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _: &tracing::span::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = BTreeMap::new();
            attrs.record(&mut Fields(&mut fields));
            let name = attrs.metadata().name().to_string();
            self.0.lock().unwrap().push((name, fields));
        }
    }

    /// What the host does over SSH for a provider is an `io.call` span,
    /// as a request it sends is: named by the provider and what it does,
    /// where it goes a field, without the user.
    #[test]
    fn an_ssh_call_is_a_span_of_its_own() {
        use tracing_subscriber::layer::SubscriberExt;
        let begun = Begun::default();
        let subscriber = tracing_subscriber::registry().with(begun.clone());
        let on = Target {
            host: "10.0.0.5".into(),
            user: "ubuntu".into(),
            port: None,
        };
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let host = Services::new(Grants::none("k3s"));
            let to = Endpoint {
                host: "127.0.0.1".into(),
                port: 6443,
            };
            assert!(host.exec(&on, &["true".into()], None).is_err());
            assert!(host.write(&on, "/etc/k3s.yaml", b"", 0o600).is_err());
            assert!(host.forward(&on, &to).is_err());
        });
        let spans: Vec<(String, String, String)> = begun
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(name, f)| (name.clone(), f["otel.name"].clone(), f["addr"].clone()))
            .collect();
        let call =
            |name: &str, addr: &str| ("io.call".to_string(), name.to_string(), addr.to_string());
        assert_eq!(
            spans,
            [
                call("k3s ssh exec", "ssh://10.0.0.5"),
                call("k3s ssh write", "ssh://10.0.0.5/etc/k3s.yaml"),
                call("k3s ssh forward", "ssh://10.0.0.5"),
            ]
        );
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

    /// A signer whose token is what a command prints.
    struct Printed;

    impl Signer for Printed {
        fn needs(&self) -> Result<Vec<Need>, String> {
            Ok(vec![Need::Exec(Command {
                argv: ["sh", "-c", "printf '%s' \"$T\""]
                    .map(String::from)
                    .to_vec(),
                env: vec![("T".into(), "from-exec".into())],
            })])
        }

        fn step(&self, got: &[Got]) -> Result<Outcome, String> {
            let [Got::Exec(out)] = got else {
                return Err(format!("got {got:?}"));
            };
            let token = String::from_utf8(out.clone()).map_err(|e| e.to_string())?;
            Ok(Outcome::Material(Material {
                recipe: vec![Recipe::Bearer(token)],
                ..Material::default()
            }))
        }

        fn sign(&self, _: Request, _: &Material, _: u64) -> Result<Request, String> {
            Err("a bearer token signs nothing".into())
        }

        fn refresh(&self, m: &Material, _: u64) -> Result<Outcome, String> {
            Ok(Outcome::Material(m.clone()))
        }
    }

    /// The host runs the command a signer names, under the grant, and
    /// hands it what the command printed.
    #[test]
    fn a_signers_command_is_run_by_the_host() {
        let parts = [("token".to_string(), Secret::new(Vec::new()))];
        let held = credentials::hold(Kind::Signed, "p", parts.into());
        let host = Services::new(Grants::none("p")).with_signer(Arc::new(Printed));
        let h = host.open(held.name()).unwrap().handle;
        let c = host.credential(h).unwrap();
        assert_eq!(c.headers[0].1.expose(), b"Bearer from-exec");
    }
}
