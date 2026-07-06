use sley::plumbing::sley_core::cli_exit_code;
use sley::GitError;
fn main() {
    // Use args_os rather than env::args(), which panics (Result::unwrap) on any
    // argument that is not valid UTF-8. Git accepts arbitrary bytes on the
    // command line (e.g. `log --grep` under a non-UTF-8 output encoding).
    // Valid-UTF-8 args are byte-identical after conversion; invalid bytes are
    // preserved as private-use sentinels instead of being collapsed to U+FFFD.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(sley_cli::argv_string_from_os)
        .collect();
    if let Err(err) = sley_cli::run(args) {
        report_cli_error(&err);
        std::process::exit(cli_exit_code(&err));
    }
}

fn report_cli_error(err: &GitError) {
    match err {
        // Message was already printed by the command (e.g. `usage_error` in args.rs).
        GitError::Exit(_) => {}
        GitError::InvalidFormat(msg)
            if msg.starts_with("fatal: ") || msg.starts_with("error: ") =>
        {
            eprintln!("{msg}")
        }
        GitError::Cli(_, msg) => eprintln!("sley: {msg}"),
        _ => eprintln!("sley: {err}"),
    }
}
