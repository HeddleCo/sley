use sley_core::GitError;

fn main() {
    if let Err(err) = sley_cli::run(std::env::args().skip(1).collect()) {
        if let GitError::Exit(code) = err {
            std::process::exit(code);
        }
        eprintln!("sley: {err}");
        std::process::exit(1);
    }
}
