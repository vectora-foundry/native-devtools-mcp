pub mod install_source;
pub mod setup;
pub mod verify;

// ANSI color/style codes shared across CLI subcommands
pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RESET: &str = "\x1b[0m";

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Check argv for CLI subcommands. Returns `true` if a subcommand was handled
/// (meaning the process should exit), `false` if the MCP server should start.
pub fn handle_subcommand() -> bool {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("setup") => {
            setup::run();
            true
        }
        Some("verify") => {
            verify::run();
            true
        }
        Some("--version" | "-V") => {
            println!("native-devtools-mcp {VERSION}");
            true
        }
        Some("--help" | "-h" | "help") => {
            print_help();
            true
        }
        Some(unknown) if unknown.starts_with('-') || unknown.starts_with("--") => {
            eprintln!("Unknown option: {unknown}");
            eprintln!();
            print_help();
            std::process::exit(1);
        }
        Some(unknown) => {
            eprintln!("Unknown command: {unknown}");
            eprintln!();
            print_help();
            std::process::exit(1);
        }
        None => false,
    }
}

fn print_help() {
    println!(
        "\
native-devtools-mcp {VERSION}

USAGE:
    native-devtools-mcp [COMMAND]

COMMANDS:
    setup     Guided setup for permissions and MCP client configuration
    verify    Verify binary integrity against GitHub release checksums

OPTIONS:
    -h, --help       Print this help message
    -V, --version    Print version

When run without a command, starts the MCP server (stdio transport)."
    );
}
