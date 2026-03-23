use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    if KEYBOARD_ENHANCED.load(Ordering::Relaxed) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), crossterm::cursor::Show);
}

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
    let project_raw = cli.project.unwrap_or_else(|| {
        std::env::current_dir()
            .expect("cannot get current directory")
            .to_string_lossy()
            .into_owned()
    });
    let project_name = Path::new(&project_raw)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&project_raw);
    info!("using project {} ({})", project_name, project_raw);
    let project = Some(project_raw);

    // Connect to nerve
    let (client, event_rx) = NerveClient::connect(&url, &cli.name).await?;
    info!("connected to nerve at {}", url);

    // Install panic hook to restore terminal on panic
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_panic(info);
    }));

    // Setup terminal
    enable_raw_mode()?;

    // Enable kitty keyboard protocol for Shift+Enter detection (if terminal supports it)
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        );
        KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
    }

    // Handle SIGINT (e.g. external kill -2) to restore terminal before exit
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            restore_terminal();
            std::process::exit(130); // 128 + SIGINT(2)
        }
    });

    // Run setup + app inside closure so ? won't skip cleanup
    let result = async {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut app = App::new_with_project(client, event_rx, project);
        app.init().await?;
        app.run(&mut terminal).await
    }
    .await;

    // Restore terminal (always runs, even if setup/init/run failed)
    restore_terminal();

    result
}
