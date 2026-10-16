//! A provider's signer over gRPC (`Signer`, the WIT's `signer`): a native
//! provider that signs serves it ([`Served`], `server::Declares::signer`),
//! and dform's host calls it ([`Remote`]) to make a `signed` credential's
//! material and to sign each request sent with it. The messages carry the
//! WIT's values as they are; a failure is the answer's `error`.

use crate::host_pb as h;
use dform_core::plugin::signer::{
    Got, Material, Need, Outcome, Recipe, Request, Response, Signer, TlsClient,
};
use std::sync::Arc;
use tonic::transport::Channel;

fn headers(hs: Vec<(String, String)>) -> Vec<h::Header> {
    hs.into_iter()
        .map(|(name, value)| h::Header { name, value })
        .collect()
}

fn pairs(hs: Vec<h::Header>) -> Vec<(String, String)> {
    hs.into_iter().map(|h| (h.name, h.value)).collect()
}

impl From<Request> for h::SignerRequest {
    fn from(r: Request) -> h::SignerRequest {
        h::SignerRequest {
            method: r.method,
            url: r.url,
            headers: headers(r.headers),
            body: r.body,
        }
    }
}

impl From<h::SignerRequest> for Request {
    fn from(r: h::SignerRequest) -> Request {
        Request {
            method: r.method,
            url: r.url,
            headers: pairs(r.headers),
            body: r.body,
        }
    }
}

impl From<Need> for h::Need {
    fn from(n: Need) -> h::Need {
        use h::need::{Need as N, Strings};
        h::Need {
            need: Some(match n {
                Need::Env(values) => N::Env(Strings { values }),
                Need::File(path) => N::File(path),
                Need::Exec(values) => N::Exec(Strings { values }),
                Need::Fetch(r) => N::Fetch(r.into()),
            }),
        }
    }
}

impl TryFrom<h::Need> for Need {
    type Error = String;

    fn try_from(n: h::Need) -> Result<Need, String> {
        use h::need::Need as N;
        Ok(match n.need.ok_or("a need that is none")? {
            N::Env(s) => Need::Env(s.values),
            N::File(path) => Need::File(path),
            N::Exec(s) => Need::Exec(s.values),
            N::Fetch(r) => Need::Fetch(r.into()),
        })
    }
}

impl From<Got> for h::Got {
    fn from(g: Got) -> h::Got {
        use h::got::{Fetched, File, Got as G};
        h::Got {
            got: Some(match g {
                Got::Env(vars) => G::Env(h::Headers {
                    values: headers(vars),
                }),
                Got::File(data) => G::File(File { data }),
                Got::Exec(out) => G::Exec(out),
                Got::Fetch(r) => G::Fetch(Fetched {
                    status: r.status.into(),
                    headers: headers(r.headers),
                    body: r.body,
                    at: r.at,
                }),
            }),
        }
    }
}

impl TryFrom<h::Got> for Got {
    type Error = String;

    fn try_from(g: h::Got) -> Result<Got, String> {
        use h::got::Got as G;
        Ok(match g.got.ok_or("a got that is none")? {
            G::Env(vars) => Got::Env(pairs(vars.values)),
            G::File(f) => Got::File(f.data),
            G::Exec(out) => Got::Exec(out),
            G::Fetch(r) => Got::Fetch(Response {
                status: u16::try_from(r.status).map_err(|_| "a status past 65535")?,
                headers: pairs(r.headers),
                body: r.body,
                at: r.at,
            }),
        })
    }
}

impl From<Material> for h::Material {
    fn from(m: Material) -> h::Material {
        use h::material::{Recipe as R, Signed, TlsClient as T};
        h::Material {
            bytes: m.bytes,
            recipe: Some(match m.recipe {
                Recipe::Bearer(t) => R::Bearer(t),
                Recipe::Headers(hs) => R::Headers(h::Headers {
                    values: headers(hs),
                }),
                Recipe::Signed => R::Signed(Signed {}),
                Recipe::TlsClient(t) => R::TlsClient(T {
                    cert: t.cert,
                    key: t.key,
                    ca: t.ca,
                }),
                Recipe::Proxy(p) => R::Proxy(p),
            }),
            expires: m.expires,
        }
    }
}

impl TryFrom<h::Material> for Material {
    type Error = String;

    fn try_from(m: h::Material) -> Result<Material, String> {
        use h::material::Recipe as R;
        Ok(Material {
            bytes: m.bytes,
            recipe: match m.recipe.ok_or("material with no recipe")? {
                R::Bearer(t) => Recipe::Bearer(t),
                R::Headers(hs) => Recipe::Headers(pairs(hs.values)),
                R::Signed(_) => Recipe::Signed,
                R::TlsClient(t) => Recipe::TlsClient(TlsClient {
                    cert: t.cert,
                    key: t.key,
                    ca: t.ca,
                }),
                R::Proxy(p) => Recipe::Proxy(p),
            },
            expires: m.expires,
        })
    }
}

/// A step's outcome as the answer carries it.
fn stepped(r: Result<Outcome, String>) -> h::StepResponse {
    match r {
        Ok(Outcome::More(needs)) => h::StepResponse {
            more: needs.into_iter().map(Into::into).collect(),
            ..Default::default()
        },
        Ok(Outcome::Material(m)) => h::StepResponse {
            material: Some(m.into()),
            ..Default::default()
        },
        Err(e) => h::StepResponse {
            error: Some(e),
            ..Default::default()
        },
    }
}

/// The outcome an answer carries.
fn outcome(r: h::StepResponse) -> Result<Outcome, String> {
    if let Some(e) = r.error {
        return Err(e);
    }
    match r.material {
        Some(m) => Ok(Outcome::Material(m.try_into()?)),
        None => Ok(Outcome::More(
            r.more
                .into_iter()
                .map(Need::try_from)
                .collect::<Result<_, _>>()?,
        )),
    }
}

/// A provider's signer, served.
pub struct Served(pub Arc<dyn Signer + Send + Sync>);

type R<T> = Result<tonic::Response<T>, tonic::Status>;

#[tonic::async_trait]
impl h::signer_server::Signer for Served {
    async fn needs(&self, _: tonic::Request<h::NeedsRequest>) -> R<h::NeedsResponse> {
        let needs = self.0.needs().map_err(tonic::Status::failed_precondition)?;
        Ok(tonic::Response::new(h::NeedsResponse {
            needs: needs.into_iter().map(Into::into).collect(),
        }))
    }

    async fn step(&self, r: tonic::Request<h::StepRequest>) -> R<h::StepResponse> {
        let got: Result<Vec<Got>, String> =
            r.into_inner().got.into_iter().map(Got::try_from).collect();
        Ok(tonic::Response::new(stepped(
            got.and_then(|got| self.0.step(&got)),
        )))
    }

    async fn sign(&self, r: tonic::Request<h::SignRequest>) -> R<h::SignResponse> {
        let r = r.into_inner();
        let signed = Material::try_from(r.material.unwrap_or_default()).and_then(|m| {
            let req = r.request.unwrap_or_default().into();
            self.0.sign(req, &m, r.now)
        });
        Ok(tonic::Response::new(match signed {
            Ok(req) => h::SignResponse {
                request: Some(req.into()),
                error: None,
            },
            Err(e) => h::SignResponse {
                request: None,
                error: Some(e),
            },
        }))
    }

    async fn refresh(&self, r: tonic::Request<h::RefreshRequest>) -> R<h::StepResponse> {
        let r = r.into_inner();
        let m = Material::try_from(r.material.unwrap_or_default());
        Ok(tonic::Response::new(stepped(
            m.and_then(|m| self.0.refresh(&m, r.now)),
        )))
    }
}

/// A native provider's `Signer`, by where it listens: dform's host's
/// signer of that provider, each call dialed and answered on a runtime of
/// its own, as `Io`'s are (a call comes from a thread of the host's
/// blocking pool).
pub struct Remote {
    address: String,
    program: String,
}

impl Remote {
    pub fn new(address: &str, program: &str) -> Remote {
        Remote {
            address: address.to_string(),
            program: program.to_string(),
        }
    }

    /// `f` of the provider's `Signer` client, its failure named.
    fn call<T>(
        &self,
        what: &str,
        f: impl AsyncFnOnce(&mut h::signer_client::SignerClient<Channel>) -> Result<T, tonic::Status>,
    ) -> Result<T, String> {
        let failed = |e: String| format!("provider {}'s signer: {what}: {e}", self.program);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| failed(e.to_string()))?;
        rt.block_on(async {
            let channel = crate::transport::dial(&self.address)
                .await
                .map_err(|e| format!("{e:#}"))?;
            let mut client = h::signer_client::SignerClient::new(channel);
            f(&mut client).await.map_err(|s| s.message().to_string())
        })
        .map_err(failed)
    }
}

impl Signer for Remote {
    fn needs(&self) -> Result<Vec<Need>, String> {
        let r = self.call("needs", async |c| c.needs(h::NeedsRequest {}).await)?;
        r.into_inner()
            .needs
            .into_iter()
            .map(Need::try_from)
            .collect()
    }

    fn step(&self, got: &[Got]) -> Result<Outcome, String> {
        let got = got.iter().cloned().map(Into::into).collect();
        let r = self.call("step", async |c| c.step(h::StepRequest { got }).await)?;
        outcome(r.into_inner())
    }

    fn sign(&self, req: Request, material: &Material, now: u64) -> Result<Request, String> {
        let asked = h::SignRequest {
            request: Some(req.into()),
            material: Some(material.clone().into()),
            now,
        };
        let r = self
            .call("sign", async |c| c.sign(asked).await)?
            .into_inner();
        match (r.error, r.request) {
            (Some(e), _) => Err(e),
            (None, Some(req)) => Ok(req.into()),
            (None, None) => Err("it answered no request".into()),
        }
    }

    fn refresh(&self, material: &Material, now: u64) -> Result<Outcome, String> {
        let asked = h::RefreshRequest {
            material: Some(material.clone().into()),
            now,
        };
        let r = self.call("refresh", async |c| c.refresh(asked).await)?;
        outcome(r.into_inner())
    }
}
