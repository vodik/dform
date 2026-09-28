//! How a provider and dform reach each other: TCP on the loopback
//! interface, or a unix socket. The provider picks and names it in its
//! handshake line (`spawn::parse_handshake`); dform dials what it names.
//!
//! A provider built on `serve` listens on TCP unless
//! `DFORM_PROVIDER_TRANSPORT=unix` is in its environment (dform's, which it
//! inherits): then on a socket in the temporary directory, removed when
//! the provider exits or dform closes the connection.

use anyhow::{Result, bail};
use std::path::Path;
use tonic::transport::server::Router;
use tonic::transport::{Channel, Endpoint, Uri};

/// The environment variable that chooses a provider's transport: `tcp`
/// (the default) or `unix`.
pub const ENV: &str = "DFORM_PROVIDER_TRANSPORT";

/// The unix socket a `unix://PATH` address names.
pub fn socket(address: &str) -> Option<&Path> {
    address.strip_prefix("unix://").map(Path::new)
}

/// Dial the provider at `address`: an `http://HOST:PORT` URI, or
/// `unix://PATH`.
pub async fn dial(address: &str) -> Result<Channel> {
    let Some(path) = socket(address) else {
        return Ok(Endpoint::from_shared(address.to_string())?
            .connect()
            .await?);
    };
    let path = path.to_path_buf();
    // The URI only names the HTTP/2 authority; the connector opens the
    // socket.
    let channel = Endpoint::from_static("http://provider")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(&path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(channel)
}

/// Serve `router` as a provider: listen (`ENV`), print the handshake line
/// naming where, serve until stdin closes, then exit.
pub fn serve(router: Router) -> Result<()> {
    let socket = match std::env::var(ENV).as_deref() {
        Ok("unix") => {
            Some(std::env::temp_dir().join(format!("dform-provider-{}.sock", std::process::id())))
        }
        Ok("tcp") | Err(_) => None,
        Ok(other) => bail!("{ENV}={other}: expected tcp or unix"),
    };
    let gone = socket.clone();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        if let Some(p) = gone {
            let _ = std::fs::remove_file(p);
        }
        std::process::exit(0);
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        match socket {
            Some(path) => {
                let _ = std::fs::remove_file(&path);
                let listener = tokio::net::UnixListener::bind(&path)?;
                handshake(&format!("unix://{}", path.display()))?;
                router
                    .serve_with_incoming(
                        tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(listener),
                    )
                    .await?;
            }
            None => {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let port = listener.local_addr()?.port();
                handshake(&format!("tcp://127.0.0.1:{port}"))?;
                router
                    .serve_with_incoming(
                        tonic::transport::server::TcpIncoming::from(listener)
                            .with_nodelay(Some(true)),
                    )
                    .await?;
            }
        }
        Ok(())
    })
}

/// `dform-provider|VERSION|ADDRESS` on stdout.
fn handshake(address: &str) -> Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout();
    writeln!(
        out,
        "{}|{}|{address}",
        super::spawn::MAGIC,
        super::spawn::VERSION
    )?;
    out.flush()?;
    Ok(())
}

/// Remove the socket of `address` once its provider is gone.
pub fn remove(address: &str) {
    if let Some(p) = socket(address) {
        let _ = std::fs::remove_file(p);
    }
}
