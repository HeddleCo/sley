use git_core::GitError;

fn main() {
    if let Err(err) = git_cli::run(std::env::args().skip(1).collect()) {
        if let GitError::Exit(code) = err {
            std::process::exit(code);
        }
        eprintln!("git-rs: {err}");
        std::process::exit(1);
    }
}
