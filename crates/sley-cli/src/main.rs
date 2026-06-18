use sley_core::{GitError, cli_exit_code};

fn main() {
    if let Err(err) = sley_cli::run(std::env::args().skip(1).collect()) {
        report_cli_error(&err);
        std::process::exit(cli_exit_code(&err));
    }
}

fn report_cli_error(err: &GitError) {
    match err {
        // Message was already printed by the command (e.g. `usage_error` in args.rs).
        GitError::Exit(_) => {}
        GitError::InvalidFormat(msg) if msg.starts_with("fatal: ") => eprintln!("{msg}"),
        GitError::Cli(_, msg) => eprintln!("sley: {msg}"),
        _ => eprintln!("sley: {err}"),
    }
}
