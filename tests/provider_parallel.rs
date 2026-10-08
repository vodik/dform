//! R-142: an SDK provider answers the calls dform sends at once at once,
//! and its calls to the host run at once, through every layer: the
//! protocol's adapter (`dform_grpc::server::Adapter` on the transport's
//! single-threaded runtime), the SDK's host client and dform's host
//! service. The provider's Read asks the host for a URL whose server
//! answers only once two requests are open at the same time; two Reads
//! sent at once both find it.
//!
//! The SDK finds the host in `DFORM_HOST`, as a provider dform starts
//! does: this binary sets it, so it holds this one test.

use dform::plugin::backend::{Call, CallError, Handler, Progress, Reply};
use dform::plugin::host::Grants;
use dform::plugin::pb;
use dform_grpc::pb::provider_client::ProviderClient;
use dform_grpc::pb::provider_server::ProviderServer;
use dform_host::services::Services;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

mod common;

/// Where the provider's Reads go.
static URL: OnceLock<String> = OnceLock::new();

/// Reads `URL` through the host; found when it answered `200 pair`.
struct Fetches;

impl Handler for Fetches {
    fn handle(&self, call: Call, _: Progress) -> Result<Reply, CallError> {
        match call {
            Call::Read(_) => {
                let r = dform_sdk::host()
                    .http
                    .request(dform_sdk::Request::get(URL.get().unwrap()))
                    .map_err(|e| CallError::Refused(e.message))?;
                Ok(Reply::Read(pb::ReadResponse {
                    found: r.status == 200,
                    ..Default::default()
                }))
            }
            other => Err(CallError::Refused(format!("{} only", other.method()))),
        }
    }
}

/// The adapter served as `transport::serve` serves it (a single-threaded
/// runtime on a thread of its own), without its stdin watcher; its
/// address.
fn serve(handler: Fetches) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    l.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(l).unwrap();
            tonic::transport::Server::builder()
                .add_service(ProviderServer::new(dform_grpc::server::Adapter::new(
                    handler,
                )))
                .serve_with_incoming(tonic::transport::server::TcpIncoming::from(l))
                .await
                .unwrap();
        });
    });
    format!("http://{addr}")
}

#[test]
fn an_sdk_provider_answers_two_reads_at_once() {
    let host = dform_host::grpc::serve(Services::new(Grants::none("fetches"))).unwrap();
    // SAFETY: set before any thread of this binary reads the environment
    // (the only test in it, before it starts any).
    unsafe { std::env::set_var("DFORM_HOST", &host.address) };
    let pairs = common::answers_in_pairs(Duration::from_secs(3));
    URL.set(format!("http://{pairs}/")).unwrap();
    let provider = serve(Fetches);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let from = Instant::now();
    let (a, b) = rt.block_on(async move {
        let c = ProviderClient::connect(provider).await.unwrap();
        let read = |name: &str| {
            let mut c = c.clone();
            let req = pb::ReadRequest {
                r#type: "web.page".into(),
                remote: name.into(),
                name: name.into(),
            };
            async move { c.read(req).await.map(|r| r.into_inner().found) }
        };
        tokio::join!(read("a"), read("b"))
    });
    let took = from.elapsed();
    assert_eq!(
        (a.map_err(|s| s.to_string()), b.map_err(|s| s.to_string())),
        (Ok(true), Ok(true)),
        "both Reads find the server with the other's request open ({took:?})"
    );
}
