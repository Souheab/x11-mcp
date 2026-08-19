mod safety;
mod server;
mod session;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use atspi_controller::AccessibilityMode;
use clap::{Parser, ValueEnum};
use rmcp::{ServiceExt as _, transport::stdio};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use x11_controller::{ControllerConfig, DesktopController};

use crate::{safety::validate_target_display, server::X11McpServer, session::DesktopSession};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AccessibilityModeArg {
    Auto,
    Disabled,
    Required,
}

impl From<AccessibilityModeArg> for AccessibilityMode {
    fn from(value: AccessibilityModeArg) -> Self {
        match value {
            AccessibilityModeArg::Auto => Self::Auto,
            AccessibilityModeArg::Disabled => Self::Disabled,
            AccessibilityModeArg::Required => Self::Required,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Display-scoped X11 controller exposed over MCP stdio"
)]
struct Cli {
    /// X11 display to control, for example :20.
    #[arg(long)]
    display: String,

    /// Xauthority file to use instead of XAUTHORITY or ~/.Xauthority.
    #[arg(long)]
    xauthority: Option<PathBuf>,

    /// Permit controlling the inherited or likely host desktop display.
    #[arg(long)]
    allow_host_display: bool,

    /// Glob matching an allowed `WM_CLASS` instance or class. Repeatable.
    #[arg(long = "allow-window-class")]
    allow_window_classes: Vec<String>,

    /// Maximum synthesized XTEST events accepted in a one-second burst.
    #[arg(long, default_value_t = 200)]
    max_input_events_per_second: u32,

    /// Key chord used by clipboard text insertion.
    #[arg(long, default_value = "CTRL+V")]
    paste_chord: String,

    /// AT-SPI availability policy.
    #[arg(long, value_enum, default_value = "auto")]
    accessibility: AccessibilityModeArg,

    /// stderr tracing filter.
    #[arg(
        long,
        env = "RUST_LOG",
        default_value = "x11_mcp=info,x11_controller=info"
    )]
    log_level: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_target_display(
        &cli.display,
        std::env::var("DISPLAY").ok().as_deref(),
        cli.allow_host_display,
    )?;
    if cli.max_input_events_per_second == 0 {
        bail!("--max-input-events-per-second must be greater than zero");
    }
    if let Some(path) = &cli.xauthority {
        if !path.is_file() {
            bail!("Xauthority file does not exist: {}", path.display());
        }
        set_xauthority_before_threads(path);
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log_level).context("invalid --log-level filter")?)
        .with_writer(std::io::stderr)
        .with_target(true)
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime")?;
    runtime.block_on(run(cli))
}

#[allow(unsafe_code)]
fn set_xauthority_before_threads(path: &std::path::Path) {
    // SAFETY: main calls this before constructing Tokio or spawning the X11 actor. No other thread
    // can concurrently read or modify the process environment at this point.
    unsafe { std::env::set_var("XAUTHORITY", path) };
}

async fn run(cli: Cli) -> Result<()> {
    let paste_chord = cli
        .paste_chord
        .split('+')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if paste_chord.is_empty() {
        bail!("--paste-chord must contain at least one key");
    }
    let emergency_stop = Arc::new(AtomicBool::new(false));
    install_emergency_stop(emergency_stop.clone())?;
    let controller = x11_controller::connect(
        ControllerConfig {
            display: cli.display.clone(),
            allow_window_classes: cli.allow_window_classes,
            max_input_events_per_second: cli.max_input_events_per_second,
            paste_chord,
        },
        emergency_stop,
    )?;
    let controller: Arc<dyn DesktopController> = Arc::new(controller);
    let capabilities = controller.capabilities().await?;
    info!(display = %capabilities.display, "connected to X11 display; starting MCP stdio");
    let session = Arc::new(
        DesktopSession::new(controller, cli.accessibility.into())
            .await
            .map_err(anyhow::Error::msg)?,
    );
    let service = X11McpServer::new(session)
        .serve(stdio())
        .await
        .context("start MCP stdio service")?;
    service
        .waiting()
        .await
        .context("MCP stdio service stopped")?;
    Ok(())
}

fn install_emergency_stop(stop: Arc<AtomicBool>) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut signal = signal(SignalKind::user_defined1()).context("install SIGUSR1 handler")?;
        tokio::spawn(async move {
            if signal.recv().await.is_some() {
                stop.store(true, Ordering::SeqCst);
                warn!("SIGUSR1 received: emergency stop latched; restart to re-enable input");
            }
        });
    }
    #[cfg(not(unix))]
    let _ = stop;
    Ok(())
}
