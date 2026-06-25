use sley_core::{GitError, cli_exit_code};

fn main() {
    // Use args_os + lossy conversion rather than env::args(), which panics
    // (Result::unwrap) on any argument that is not valid UTF-8. Git accepts
    // arbitrary bytes on the command line (e.g. `commit -m` with a non-UTF-8
    // message under i18n.commitencoding); a hard panic there is strictly worse
    // than a lossy decode. Valid-UTF-8 args are byte-identical after the
    // conversion, so this never changes behaviour for well-formed input.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
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
