//! Defines the Rojo web interface. This is what the Roblox Studio plugin
//! communicates with. Eventually, we'll make this API stable, produce better
//! documentation for it, and open it up for other consumers.

mod api;
mod assets;
pub mod interface;
mod origin;
mod ui;
mod util;

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use hyper::{
    server::{conn::AddrStream, Server},
    service::{make_service_fn, service_fn},
    Body, Method, Request, StatusCode,
};
use tokio::runtime::{Handle, Runtime};

use crate::{serve_session::ServeSession, server_registry};

pub struct LiveServer {
    serve_session: Arc<ServeSession>,
}

impl LiveServer {
    pub fn new(serve_session: Arc<ServeSession>) -> Self {
        LiveServer { serve_session }
    }

    /// Starts the server on the given address, blocking until it stops.
    ///
    /// `allowed_hosts` are extra `Host`/`Origin` values to accept in addition to
    /// localhost and the bind address (see [`origin::allowed_hosts`]).
    ///
    /// `on_listening` is invoked once the server has successfully bound to the
    /// address, so callers can defer printing any "listening" message until
    /// after binding can no longer fail (e.g. due to the port being in use).
    pub fn start(
        self,
        address: SocketAddr,
        allowed_hosts: Vec<String>,
        on_listening: impl FnOnce(),
    ) -> anyhow::Result<()> {
        let serve_session = Arc::clone(&self.serve_session);
        let allowed_hosts = origin::allowed_hosts(address.ip(), address.port(), &allowed_hosts);

        let make_service = make_service_fn(move |conn: &AddrStream| {
            let serve_session = Arc::clone(&serve_session);
            let allowed_hosts = allowed_hosts.clone();
            let remote_addr = conn.remote_addr();

            async move {
                let service = move |req: Request<Body>| {
                    let serve_session = Arc::clone(&serve_session);
                    let allowed_hosts = allowed_hosts.clone();

                    async move {
                        // Reject cross-origin requests before doing any work, to
                        // defend the local server against DNS rebinding.
                        if let Some(response) =
                            origin::check_request_origin(&req, allowed_hosts.as_ref())
                        {
                            return Ok::<_, Infallible>(response);
                        }

                        if req.uri().path().starts_with("/api") {
                            Ok::<_, Infallible>(api::call(serve_session, remote_addr, req).await)
                        } else {
                            Ok::<_, Infallible>(ui::call(serve_session, req).await)
                        }
                    }
                };

                Ok::<_, Infallible>(service_fn(service))
            }
        });

        let rt = Runtime::new().context("Failed to start the async runtime for the web server")?;
        let _guard = rt.enter();
        let server = Server::try_bind(&address)
            .with_context(|| {
                format!(
                    "Could not start the Rojo server on {address}.\n\
                     The address may already be in use or reserved. Another Rojo server might already \
                     be running, or another program may be using that port.\n\
                     You can pick a different port with the --port option."
                )
            })?
            .serve(make_service);

        // Binding succeeded, so it's now safe to tell the user we're listening.
        on_listening();

        let handle = rt.handle().clone();
        std::thread::Builder::new()
            .name("Rojo discovery".to_owned())
            .spawn(move || hold_discovery_port(handle))
            .context("Could not start listening on the discovery port")?;

        rt.block_on(server)
            .context("The Rojo web server encountered a fatal error")?;

        Ok(())
    }
}

/// How often a server that doesn't hold the discovery port tries to take it
/// over, which it can once the server holding it stops.
const DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// Listens on the discovery port whenever no other Rojo server does, so the
/// plugin can always find the list of running servers there (see
/// [`server_registry`]). Runs for as long as the process does.
fn hold_discovery_port(handle: Handle) {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, server_registry::DISCOVERY_PORT));
    let allowed_hosts = origin::allowed_hosts(address.ip(), address.port(), &[]);

    loop {
        // Bound with the standard library rather than hyper so that a port
        // another server holds fails right away, without logging an error.
        if let Ok(listener) = std::net::TcpListener::bind(address) {
            log::debug!("Listening for Rojo server discovery on {address}");

            let allowed_hosts = allowed_hosts.clone();
            let result = handle.block_on(async move {
                listener.set_nonblocking(true)?;

                let make_service = make_service_fn(move |_: &AddrStream| {
                    let allowed_hosts = allowed_hosts.clone();

                    async move {
                        Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                            let allowed_hosts = allowed_hosts.clone();

                            async move {
                                if let Some(response) =
                                    origin::check_request_origin(&req, allowed_hosts.as_ref())
                                {
                                    return Ok::<_, Infallible>(response);
                                }

                                Ok(match (req.method(), req.uri().path()) {
                                    (&Method::GET, "/api/servers") => {
                                        api::handle_api_servers().await
                                    }
                                    _ => util::response(
                                        StatusCode::NOT_FOUND,
                                        "text/plain",
                                        "Only /api/servers is served on the discovery port",
                                    ),
                                })
                            }
                        }))
                    }
                });

                Server::from_tcp(listener)?.serve(make_service).await?;
                anyhow::Ok(())
            });

            if let Err(err) = result {
                log::debug!("Stopped listening for Rojo server discovery: {err:#}");
            }
        }

        std::thread::sleep(DISCOVERY_RETRY_INTERVAL);
    }
}
