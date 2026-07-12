use sley::GitError;
use sley::plumbing::sley_core::cli_exit_code;

fn main() {
    let args = std::env::args_os()
        .skip(1)
        .map(sley_cli::argv_string_from_os)
        .collect();
    if let Err(err) = sley_cli::run_scalar(args) {
        report_cli_error(&err);
        std::process::exit(cli_exit_code(&err));
    }
}

fn report_cli_error(err: &GitError) {
    match err {
        GitError::Exit(_) => {}
        GitError::InvalidFormat(msg)
            if msg.starts_with("fatal: ") || msg.starts_with("error: ") =>
        {
            eprintln!("{msg}")
        }
        GitError::Cli(_, msg) => eprintln!("scalar: {msg}"),
        _ => eprintln!("scalar: {err}"),
    }
}
