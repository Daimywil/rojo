use std::{
    io::{self, BufRead, Write},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use clap::{ArgEnum, Parser};
use futures::channel::oneshot;
use memofs::Vfs;
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};

use crate::{
    serve_events::{self, ServeEvent, ShutdownReason},
    serve_session::ServeSession,
    server_registry::{self, ServerEntry},
    web::{interface::PROTOCOL_VERSION, LiveServer},
};

use super::{resolve_path, GlobalOptions};

const DEFAULT_BIND_ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const DEFAULT_PORT: u16 = 34872;

/// Expose a Rojo project to the Rojo Studio plugin.
#[derive(Debug, Parser)]
pub struct ServeCommand {
    /// Path to the project to serve. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// The IP address to listen on. Defaults to `127.0.0.1`.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// The port to listen on. Defaults to the project's preference, or `34872` if
    /// it has none.
    #[clap(long)]
    pub port: Option<u16>,

    /// Extra `Host`/`Origin` values the server will accept, beyond localhost and
    /// the bind address (for example a hostname like `mypc.lan`). Repeat the
    /// option or comma-separate to allow several. When given, this overrides the
    /// project's `serveAllowedHosts`. Listing any host also turns on Host/Origin
    /// validation for binds where it is otherwise off (such as `0.0.0.0`).
    #[clap(long, value_delimiter = ',')]
    pub allowed_hosts: Vec<String>,

    /// Print what the server does as events, for programs that run Rojo.
    /// With `json`, every event is one line on stdout: `@rojo-event `
    /// followed by a JSON object. A line saying `shutdown` on stdin then
    /// stops the server.
    #[clap(long, arg_enum, value_name = "FORMAT")]
    pub events: Option<EventFormat>,
}

/// How `rojo serve --events` prints events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ArgEnum)]
pub enum EventFormat {
    Json,
}

impl ServeCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        if self.events.is_some() {
            serve_events::enable();
        }

        let result = self.serve(global);

        // Whoever runs Rojo learns why it stops from its events, without
        // having to make sense of what's logged for people.
        serve_events::emit(&ServeEvent::Shutdown {
            reason: match &result {
                Ok(()) => ShutdownReason::Requested,
                Err(_) => ShutdownReason::Error,
            },
            message: result.as_ref().err().map(|err| format!("{err:#}")),
        });

        result
    }

    fn serve(self, global: GlobalOptions) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project)?;

        let vfs = Vfs::new_default()?;

        let session = Arc::new(ServeSession::new(vfs, project_path)?);

        // Index asset directories up front, so the first sync doesn't have to
        // wait for it, and so they're watched from the start.
        session.asset_store().scan_all()?;

        let ip = self
            .address
            .or_else(|| session.serve_address())
            .unwrap_or(DEFAULT_BIND_ADDRESS.into());

        let port = self
            .port
            .or_else(|| session.project_port())
            .unwrap_or(DEFAULT_PORT);

        // The CLI flag, when given, replaces the project's list rather than
        // merging with it, matching how --address and --port override theirs.
        let allowed_hosts = if self.allowed_hosts.is_empty() {
            session.serve_allowed_hosts().to_vec()
        } else {
            self.allowed_hosts
        };

        let entry = ServerEntry::new(
            port,
            ip,
            session.project_name().to_owned(),
            session.root_project().file_location.clone(),
        );
        let listening = ServeEvent::Listening {
            port,
            address: ip,
            project_name: session.project_name().to_owned(),
            serve_place_ids: session.sorted_serve_place_ids(),
            session_id: session.session_id(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
        };
        let server = LiveServer::new(Arc::clone(&session));

        // Kept until the server stops, so the plugin can find it (see
        // `server_registry`).
        let mut registration = None;

        // Without events, nothing ever asks the server to stop, and it runs
        // until the process is ended, like it always has. The sender is kept
        // here either way, because the server would stop if it were dropped.
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let shutdown_sender = Arc::new(Mutex::new(Some(shutdown_sender)));
        if serve_events::is_enabled() {
            listen_for_shutdown(Arc::clone(&shutdown_sender));
        }

        server.start(
            (ip, port).into(),
            allowed_hosts,
            || {
                let _ = show_start_message(ip, port, global.color.into());

                match server_registry::register(&entry) {
                    Ok(registered) => registration = Some(registered),
                    Err(err) => log::warn!(
                        "Could not register this server, so the Studio plugin won't list it: {err}"
                    ),
                }

                serve_events::emit(&listening);
            },
            async {
                let _ = shutdown_receiver.await;
            },
        )?;

        // Only reached when the server was asked to stop, which unlike being
        // killed leaves time to save what asset sync remembers.
        drop(shutdown_sender);
        session.asset_store().flush();
        drop(registration);
        Ok(())
    }
}

/// Asks the server to stop once a line saying `shutdown` arrives on stdin.
///
/// A program that runs Rojo can't always send it a signal, least of all on
/// Windows, and ending the process doesn't let the server say that it stops.
/// Stdin being closed is not taken as a request to stop, since it's closed
/// from the start when Rojo is run without one.
fn listen_for_shutdown(sender: Arc<Mutex<Option<oneshot::Sender<()>>>>) {
    let spawned = std::thread::Builder::new()
        .name("Rojo stdin".to_owned())
        .spawn(move || {
            for line in io::stdin().lock().lines() {
                match line {
                    Ok(line) if line.trim() == "shutdown" => {
                        if let Some(sender) = sender.lock().unwrap().take() {
                            let _ = sender.send(());
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });

    if let Err(err) = spawned {
        log::warn!("Could not listen for a shutdown request on stdin: {err}");
    }
}

fn show_start_message(bind_address: IpAddr, port: u16, color: ColorChoice) -> io::Result<()> {
    let mut green = ColorSpec::new();
    green.set_fg(Some(Color::Green)).set_bold(true);

    let writer = BufferWriter::stdout(color);
    let mut buffer = writer.buffer();

    let address_string = if bind_address.is_loopback() {
        "localhost".to_owned()
    } else {
        bind_address.to_string()
    };

    writeln!(&mut buffer, "Rojo server listening:")?;

    write!(&mut buffer, "  Address: ")?;
    buffer.set_color(&green)?;
    writeln!(&mut buffer, "{}", address_string)?;

    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, "  Port:    ")?;
    buffer.set_color(&green)?;
    writeln!(&mut buffer, "{}", port)?;

    writeln!(&mut buffer)?;

    if !bind_address.is_loopback() {
        let mut warning = ColorSpec::new();
        warning.set_fg(Some(Color::Yellow)).set_bold(true);

        buffer.set_color(&warning)?;
        writeln!(
            &mut buffer,
            "WARNING: This server is bound to {address_string}, which is reachable from the \
             network.\n\
             The serve API is unauthenticated, so anyone who can reach {address_string}:{port} \
             can read\n\
             and modify your project's source. Prefer binding to localhost and tunneling (e.g. \
             SSH,\n\
             Tailscale, or WireGuard) when you need remote access."
        )?;
        buffer.set_color(&ColorSpec::new())?;
        writeln!(&mut buffer)?;
    }

    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, "Visit ")?;

    buffer.set_color(&green)?;
    write!(&mut buffer, "http://{}:{}/", address_string, port)?;

    buffer.set_color(&ColorSpec::new())?;
    writeln!(&mut buffer, " in your browser for more information.")?;

    writer.print(&buffer)?;

    Ok(())
}
