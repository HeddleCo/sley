#![allow(
    dead_code,
    unused_assignments,
    unused_imports,
    unused_mut,
    unused_variables,
    clippy::all,
    clippy::unwrap_used
)]

include!("app.rs");

fn main() {
    if let Err(err) = run(std::env::args().skip(1).collect()) {
        report_cli_error(&err);
        std::process::exit(sley::plumbing::sley_core::cli_exit_code(&err));
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
