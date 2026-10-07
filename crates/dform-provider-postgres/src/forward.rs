//! A ClusterIP service reached through the Kubernetes API, as `kubectl
//! port-forward svc/NAME` does it, in process: the service's selector
//! and port, a ready pod it selects, and the API server's port-forward
//! of that pod's target port, a byte stream the client speaks Postgres
//! over. The kubeconfig is the program's (`use postgres { kubeconfig =
//! .. }`, revealed into Configure), as the k8s provider's is.

use crate::config::Forward;
use anyhow::{Context, Result, anyhow, bail};
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, ListParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use tokio::io::{AsyncRead, AsyncWrite};

/// A forwarded port: the stream, and the forwarder that carries it (kept
/// as long as the connection is).
pub struct Opened {
    pub stream: Box<dyn Duplex>,
    pub forwarder: kube::api::Portforwarder,
    /// The pod it reached, for messages.
    pub pod: String,
}

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

fn ready(p: &Pod) -> bool {
    p.status.as_ref().is_some_and(|s| {
        s.phase.as_deref() == Some("Running")
            && s.conditions
                .iter()
                .flatten()
                .any(|c| c.type_ == "Ready" && c.status == "True")
    })
}

/// The forward of `f`'s service's `port` (boxed, as `conn::connect` is).
pub fn open(
    f: &Forward,
    port: u16,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Opened>> + Send + '_>> {
    Box::pin(opened(f, port))
}

async fn opened(f: &Forward, port: u16) -> Result<Opened> {
    let at = format!("service {}/{}", f.namespace, f.service);
    let kubeconfig = Kubeconfig::from_yaml(&f.kubeconfig)
        .map_err(|_| anyhow!("provider_config postgres: kubeconfig is not a kubeconfig (YAML)"))?;
    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
        .await
        .map_err(|e| anyhow!("provider_config postgres: kubeconfig: {e}"))?;
    let client = kube::Client::try_from(config).context("a client for the kubeconfig")?;
    let services: Api<Service> = Api::namespaced(client.clone(), &f.namespace);
    let svc = services
        .get(&f.service)
        .await
        .with_context(|| format!("{at}: read it through the Kubernetes API"))?;
    let spec = svc.spec.unwrap_or_default();
    let selector = spec
        .selector
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{at} selects no pod: a port-forward reaches a pod"))?;
    let ports = spec.ports.unwrap_or_default();
    let target = ports
        .iter()
        .find(|p| p.port == i32::from(port))
        .or_else(|| (ports.len() == 1).then(|| &ports[0]))
        .ok_or_else(|| anyhow!("{at} has no port {port}"))?
        .target_port
        .clone()
        .unwrap_or(IntOrString::Int(i32::from(port)));
    let label = selector
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    let pods: Api<Pod> = Api::namespaced(client, &f.namespace);
    let params = ListParams::default().labels(&label);
    let listed = match pods.list(&params).await {
        Ok(l) => l,
        Err(e) => bail!("{at}: list the pods it selects ({label}): {e}"),
    };
    let pod = listed.items.iter().find(|p| ready(p)).ok_or_else(|| {
        anyhow!(
            "{at}: no ready pod ({} selected by {label})",
            listed.items.len()
        )
    })?;
    let name = pod.metadata.name.clone().unwrap_or_default();
    let target = match target {
        IntOrString::Int(n) => u16::try_from(n).map_err(|_| anyhow!("{at}: port {n}"))?,
        IntOrString::String(named) => pod
            .spec
            .iter()
            .flat_map(|s| s.containers.iter())
            .flat_map(|c| c.ports.iter().flatten())
            .find(|p| p.name.as_deref() == Some(named.as_str()))
            .and_then(|p| u16::try_from(p.container_port).ok())
            .ok_or_else(|| anyhow!("{at}: pod {name} names no port {named:?}"))?,
    };
    let mut forwarder = pods
        .portforward(&name, &[target])
        .await
        .with_context(|| format!("{at}: forward port {target} of pod {name}"))?;
    let Some(stream) = forwarder.take_stream(target) else {
        bail!("{at}: the forward of pod {name} gave no stream for port {target}");
    };
    Ok(Opened {
        stream: Box::new(stream),
        forwarder,
        pod: name,
    })
}
