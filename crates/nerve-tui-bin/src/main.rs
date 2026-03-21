use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::path::Path;
use tracing::info;

use nerve_tui::app::App;
use nerve_tui_core::NerveClient;

#[derive(Parser)]
#[command(name = "nerve-tui", about = "TUI client for nerve server")]
struct Cli {
    /// Nerve server address
    #[arg(short, long, default_value = "localhost:4800")]
    server: String,

    /// Node name for registration
    #[arg(short, long, default_value = "tui")]
    name: String,

    /// Project working directory for spawned agents and new channels
    #[arg(long)]
    project: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr so it doesn't interfere with TUI
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nerve_tui=info,nerve_tui_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let url = format!("ws://{}", cli.server);
    let project = cli.project.as_ref().map(|path| {
        let project_name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        info!("using project {} ({})", project_name, path);
        path.clone()
    });

    // Connect to nerve
    let (client, event_rx) = NerveClient::connect(&url, &cli.name).await?;
    info!("connected to nerve at {}", url);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let keyboard_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
    }
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let mut app = App::new_with_project(client, event_rx, project);
    app.init().await?;
    let result = app.run(&mut terminal).await;

    // Restore terminal
    disable_raw_mode()?;
    if keyboard_enhanced {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    result
}
