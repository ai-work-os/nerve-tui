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
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    let mut stdout = io::stdout();
    if KEYBOARD_ENHANCED.load(Ordering::Relaxed) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = disable_raw_mode();
    let _ = execute!(stdout, crossterm::cursor::Show);
    let _ = stdout.flush();
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
    #[arg(short, long, default_value = "user")]
    name: String,

    /// Project working directory for spawned agents and new channels
    #[arg(long)]
    project: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Log to file (~/.nerve/tui.log) to avoid polluting terminal after exit.
    // Falls back to sink (no logging) if file cannot be opened.
    let log_dir = std::env::var("HOME")
        .map(|h| Path::new(&h).join(".nerve"))
        .unwrap_or_else(|_| Path::new("/tmp").join(".nerve"));
    let log_file = std::fs::create_dir_all(&log_dir)
        .and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join("tui.log"))
        })
        .ok();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "nerve_tui=info,nerve_tui_core=info".into());
    if let Some(file) = log_file {
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_writer(io::sink)
            .with_env_filter(env_filter)
            .init();
    }

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

    // Enable kitty keyboard protocol for Shift+Enter detection.
    // crossterm's supports_keyboard_enhancement() returns false in kitty terminal,
    // so detect kitty via TERM/KITTY_WINDOW_ID and force-enable the protocol.
    let is_kitty = std::env::var("TERM").map_or(false, |t| t.contains("kitty"))
        || std::env::var("KITTY_WINDOW_ID").is_ok();
    if is_kitty || crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        if execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )
        .is_ok()
        {
            KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
        }
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
