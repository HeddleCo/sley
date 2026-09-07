use sley::GitError;
use sley_cli::cli_exit_code;

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
        _ if sley_cli::cli_reported_status(err).is_some() => {}
        GitError::InvalidFormat(msg)
            if msg.starts_with("fatal: ") || msg.starts_with("error: ") =>
        {
            eprintln!("{msg}")
        }
        _ if sley_cli::cli_message(err).is_some() => eprintln!("scalar: {err}"),
        _ => eprintln!("scalar: {err}"),
    }
}
