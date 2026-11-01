mod device;
mod frontend;
mod server;
mod webinspector;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::device::DeviceManager;
use crate::frontend::Frontend;

/// Debug Safari on a USB-connected iPhone from Linux.
#[derive(Debug, Parser)]
#[command(name = "safari-debug", version, about)]
struct Cli {
    /// Enable verbose (debug) logging.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the USB proxy and the local web inspector server.
    Serve(ServeArgs),
    /// Manage the WebKit inspector frontend assets.
    Frontend(FrontendArgs),
    /// List connected iOS devices and their debuggable pages.
    Devices,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Address to listen on.
    #[arg(short, long, default_value = "127.0.0.1:9223")]
    bind: SocketAddr,

    /// Only proxy this device UDID.
    #[arg(short, long)]
    device: Option<String>,

    /// Use an existing WebKit `UserInterface` directory as the frontend.
    #[arg(long)]
    frontend_dir: Option<PathBuf>,

    /// Do not open a browser after the server starts.
    #[arg(long)]
    no_open: bool,

    /// Do not download the WebKit frontend automatically if it is missing.
    #[arg(long)]
    no_auto_install: bool,

    /// WebKit git ref to download when installing the frontend.
    #[arg(long, default_value = "main")]
    webkit_ref: String,
}

#[derive(Debug, Args)]
struct FrontendArgs {
    #[command(subcommand)]
    command: FrontendCommand,
}

#[derive(Debug, Subcommand)]
enum FrontendCommand {
    /// Download and prepare the WebKit WebInspectorUI frontend.
    Install {
        /// WebKit git branch, tag, or ref.
        #[arg(long, default_value = "main")]
        tag: String,

        /// Re-download even if a frontend is already installed.
        #[arg(long)]
        force: bool,
    },
    /// Print the path of the installed frontend.
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command.unwrap_or(Command::Serve(ServeArgs {
        bind: "127.0.0.1:9223".parse().unwrap(),
        device: None,
        frontend_dir: None,
        no_open: false,
        no_auto_install: false,
        webkit_ref: "main".into(),
    })) {
        Command::Serve(args) => serve(args).await,
        Command::Frontend(args) => frontend_command(args).await,
        Command::Devices => list_devices().await,
    }
}

fn init_tracing(verbose: bool) {
    let default = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("safari_debug={default},idevice=warn")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

async fn frontend_command(args: FrontendArgs) -> Result<()> {
    match args.command {
        FrontendCommand::Install { tag, force } => {
            let dir = frontend::install(&tag, force).await?;
            println!("{}", dir.display());
            Ok(())
        }
        FrontendCommand::Path => {
            let dir = frontend::default_ui_dir()?;
            println!("{}", dir.display());
            Ok(())
        }
    }
}

async fn list_devices() -> Result<()> {
    let devices = device::snapshot_devices().await?;
    if devices.is_empty() {
        println!("No iOS devices found. Connect an iPhone over USB and unlock it.");
        return Ok(());
    }
    for d in devices {
        println!("{}  {}  iOS {}", d.udid, d.name, d.os_version);
        for p in d.pages {
            println!("  [{:>2}] {}  {}", p.page_num, p.title, p.url);
        }
    }
    Ok(())
}

async fn serve(args: ServeArgs) -> Result<()> {
    let frontend_dir = match args.frontend_dir {
        Some(dir) => dir,
        None => {
            let dir = frontend::default_ui_dir()?;
            if !frontend::looks_like_frontend(&dir) {
                if args.no_auto_install {
                    bail!(
                        "No WebKit inspector frontend found at {}.\n\
                         Run `safari-debug frontend install` first, or pass --frontend-dir.",
                        dir.display()
                    );
                }
                info!("WebKit inspector frontend not found; downloading it now");
                frontend::install(&args.webkit_ref, false).await?;
            }
            dir
        }
    };

    let frontend = Arc::new(Frontend::open(frontend_dir.clone())?);
    info!(
        "Serving WebKit inspector from {} (protocols: {})",
        frontend_dir.display(),
        frontend
            .protocol_versions()
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let manager = DeviceManager::new(args.device.clone());
    let cancel = CancellationToken::new();

    let manager_task = {
        let manager = manager.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { manager.run(cancel).await })
    };

    let app = server::router(Arc::new(server::AppState {
        manager: manager.clone(),
        frontend: frontend.clone(),
    }));

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("failed to bind {}", args.bind))?;
    let local_addr = listener.local_addr()?;
    let display_host = if local_addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        match local_addr.ip() {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        }
    };
    let url = format!("http://{display_host}:{}", local_addr.port());

    info!("Open {url} in your browser");
    if !args.no_open {
        open_browser(&url);
    }

    let shutdown = {
        let cancel = cancel.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("Shutting down");
            cancel.cancel();
        }
    };

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;

    cancel.cancel();
    let _ = manager_task.await;
    result.context("HTTP server failed")
}

fn open_browser(url: &str) {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["open"]
    } else if cfg!(target_os = "windows") {
        &["explorer"]
    } else {
        &["xdg-open"]
    };
    for cmd in candidates {
        match ProcessCommand::new(cmd).arg(url).spawn() {
            Ok(_) => return,
            Err(e) => warn!("could not launch {cmd}: {e}"),
        }
    }
}
