use std::{
    io::{BufWriter, Write},
    mem::forget,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use clap::{CommandFactory, Parser};
use fs_err::File;
use futures::future::Either;
use memofs::Vfs;
use roblox_install::RobloxStudio;
use tokio::runtime::Runtime;

use crate::{assets::dom_with_assets, serve_session::ServeSession};

use super::resolve_path;

const UNKNOWN_OUTPUT_KIND_ERR: &str = "Could not detect what kind of file to build. \
                                       Expected output file to end in .rbxl, .rbxlx, .rbxm, or .rbxmx.";
const UNKNOWN_PLUGIN_KIND_ERR: &str = "Could not detect what kind of file to build. \
                                       Expected plugin file to end in .rbxm or .rbxmx.";

/// Generates a model or place file from the Rojo project.
#[derive(Debug, Parser)]
pub struct BuildCommand {
    /// Path to the project to build. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Where to output the result.
    ///
    /// Should end in .rbxm, .rbxl, .rbxmx, or .rbxlx.
    #[clap(long, short, conflicts_with = "plugin")]
    pub output: Option<PathBuf>,

    /// Alternative to the output flag that outputs the result in the local plugins folder.
    ///
    /// Should end in .rbxm or .rbxl.
    #[clap(long, short, conflicts_with = "output")]
    pub plugin: Option<PathBuf>,

    /// Whether to automatically rebuild when any input files change.
    #[clap(long)]
    pub watch: bool,
}

impl BuildCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let (output_path, output_kind) = match (self.output, self.plugin) {
            (None, None) => {
                BuildCommand::command()
                    .error(
                        clap::ErrorKind::MissingRequiredArgument,
                        "one of the following arguments must be provided: \n    --output <OUTPUT>\n    --plugin <PLUGIN>",
                    )
                    .exit();
            }
            (Some(output), None) => {
                let output_kind =
                    OutputKind::from_output_path(&output).context(UNKNOWN_OUTPUT_KIND_ERR)?;

                (output, output_kind)
            }
            (None, Some(plugin)) => {
                if plugin.is_absolute() {
                    bail!("plugin flag path cannot be absolute.")
                }

                let output_kind =
                    OutputKind::from_plugin_path(&plugin).context(UNKNOWN_PLUGIN_KIND_ERR)?;
                let studio = RobloxStudio::locate()?;

                (studio.plugins_path().join(&plugin), output_kind)
            }
            _ => unreachable!(),
        };

        let project_path = resolve_path(&self.project)?;

        log::trace!("Constructing in-memory filesystem");
        let vfs = Vfs::new_default()?;
        vfs.set_watch_enabled(self.watch);

        let session = ServeSession::new(vfs, project_path)?;
        let mut cursor = session.message_queue().cursor();
        let mut asset_cursor = session.asset_store().messages().cursor();

        if self.watch {
            // Asset directories are only watched once they've been scanned.
            session.asset_store().scan_all()?;
        }

        write_model(&session, &output_path, output_kind)?;

        if self.watch {
            let rt = Runtime::new().context("Failed to start the async runtime for watch mode")?;

            loop {
                let patches = session.message_queue().subscribe(cursor);
                let assets = session.asset_store().messages().subscribe(asset_cursor);

                // Rebuild when either the instance tree or an asset changes.
                let changed = rt.block_on(async {
                    match futures::future::select(patches, assets).await {
                        Either::Left((result, _)) => result.map(|(new_cursor, _)| {
                            cursor = new_cursor;
                        }),
                        Either::Right((result, _)) => result.map(|(new_cursor, _)| {
                            asset_cursor = new_cursor;
                        }),
                    }
                });

                // A message queue was dropped, so there is nothing left to
                // watch. Stop watching gracefully.
                if changed.is_err() {
                    break;
                }

                write_model(&session, &output_path, output_kind)?;
            }
        }

        // Avoid dropping ServeSession: it's potentially VERY expensive to drop
        // and we're about to exit anyways.
        forget(session);

        Ok(())
    }
}

/// The different kinds of output that Rojo can build to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    /// An XML model file.
    Rbxmx,

    /// An XML place file.
    Rbxlx,

    /// A binary model file.
    Rbxm,

    /// A binary place file.
    Rbxl,
}

impl OutputKind {
    fn from_output_path(output: &Path) -> Option<OutputKind> {
        let extension = output.extension()?.to_str()?;

        match extension {
            "rbxlx" => Some(OutputKind::Rbxlx),
            "rbxmx" => Some(OutputKind::Rbxmx),
            "rbxl" => Some(OutputKind::Rbxl),
            "rbxm" => Some(OutputKind::Rbxm),
            _ => None,
        }
    }

    fn from_plugin_path(output: &Path) -> Option<OutputKind> {
        let extension = output.extension()?.to_str()?;

        match extension {
            "rbxmx" => Some(OutputKind::Rbxmx),
            "rbxm" => Some(OutputKind::Rbxm),
            _ => None,
        }
    }
}

fn xml_encode_config() -> rbx_xml::EncodeOptions<'static> {
    rbx_xml::EncodeOptions::new().property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown)
}

#[profiling::function]
fn write_model(
    session: &ServeSession,
    output: &Path,
    output_kind: OutputKind,
) -> anyhow::Result<()> {
    println!("Building project '{}'", session.project_name());

    let tree = session.tree();

    // Assets aren't part of the tree, so projects with asset directories are
    // built from a copy of the tree that has them added.
    let with_assets = dom_with_assets(&tree, session.asset_store())?;
    let dom = with_assets.as_ref().unwrap_or_else(|| tree.inner());
    let root_id = dom.root_ref();

    log::trace!("Opening output file for write");
    let mut file = BufWriter::new(File::create(output)?);

    match output_kind {
        OutputKind::Rbxm => {
            rbx_binary::to_writer(&mut file, dom, &[root_id])?;
        }
        OutputKind::Rbxl => {
            let top_level_ids = dom.root().children();

            rbx_binary::to_writer(&mut file, dom, top_level_ids)?;
        }
        OutputKind::Rbxmx => {
            // Model files include the root instance of the tree and all its
            // descendants.

            rbx_xml::to_writer(&mut file, dom, &[root_id], xml_encode_config())?;
        }
        OutputKind::Rbxlx => {
            // Place files don't contain an entry for the DataModel, but our
            // WeakDom representation does.

            let top_level_ids = dom.root().children();

            rbx_xml::to_writer(&mut file, dom, top_level_ids, xml_encode_config())?;
        }
    }

    file.flush()?;

    let filename = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<invalid utf-8>");
    println!("Built project to {}", filename);

    Ok(())
}
