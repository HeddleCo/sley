//! Native `git-http-backend` CGI wrapper.

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;

use sley::plumbing::sley_remote::{
    HttpBackendOperation, HttpBackendRequest, HttpBackendService, http_backend_service_enabled,
    plan_http_backend_request,
};
use sley::{GitError, Result};
use sley_protocol::write_pkt_line_payload;

use super::{cmd_receive_pack, cmd_upload_pack, ls_remote_git_dir, read_repo_config};

pub(crate) fn cmd_http_backend(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return Err(GitError::usage("usage: git http-backend"));
    }
    let path_info = env::var("PATH_INFO").ok();
    let path_translated = env::var_os("PATH_TRANSLATED").map(PathBuf::from);
    let project_root = env::var_os("GIT_PROJECT_ROOT").map(PathBuf::from);
    let method = env::var("REQUEST_METHOD").unwrap_or_default();
    let query_string = env::var("QUERY_STRING").unwrap_or_default();
    let plan = match plan_http_backend_request(HttpBackendRequest {
        method: &method,
        path_info: path_info.as_deref(),
        path_translated: path_translated.as_deref(),
        project_root: project_root.as_deref(),
        query_string: &query_string,
    }) {
        Ok(plan) => plan,
        Err(err) => return cgi_error(404, "Not Found", &err.to_string()),
    };

    let git_dir = match ls_remote_git_dir(&plan.repository.to_string_lossy()) {
        Ok(git_dir) => git_dir,
        Err(err) => return cgi_error(404, "Not Found", &err.to_string()),
    };
    if env::var_os("GIT_HTTP_EXPORT_ALL").is_none()
        && !git_dir.join("git-daemon-export-ok").exists()
    {
        return cgi_error(404, "Not Found", "repository is not exported");
    }
    let config = read_repo_config(&git_dir)?;
    let authenticated = env::var("REMOTE_USER").is_ok_and(|user| !user.is_empty());
    if !http_backend_service_enabled(plan.service, &config, authenticated) {
        return cgi_error(403, "Forbidden", "service is not enabled");
    }
    if let Some(expected) = plan.request_content_type() {
        let actual = env::var("CONTENT_TYPE").unwrap_or_default();
        if actual != expected {
            return cgi_error(
                415,
                "Unsupported Media Type",
                &format!(
                    "Expected POST with Content-Type '{expected}', but received '{actual}' instead."
                ),
            );
        }
    }

    write_success_headers(&plan.response_content_type())?;
    if plan.head_only {
        return Ok(());
    }
    let repository = plan.repository.to_string_lossy().into_owned();
    match plan.operation {
        HttpBackendOperation::Advertise => {
            if should_write_service_preamble(plan.service) {
                let stdout = io::stdout();
                let mut stdout = stdout.lock();
                write_pkt_line_payload(
                    &mut stdout,
                    format!("# service={}\n", plan.service.wire_name()).as_bytes(),
                )?;
                stdout.write_all(b"0000")?;
                stdout.flush()?;
            }
            run_service(
                plan.service,
                &["--http-backend-info-refs".into(), repository],
            )
        }
        HttpBackendOperation::Rpc => {
            run_service(plan.service, &["--stateless-rpc".into(), repository])
        }
    }
}

fn run_service(service: HttpBackendService, args: &[String]) -> Result<()> {
    match service {
        HttpBackendService::UploadPack => cmd_upload_pack(args),
        HttpBackendService::ReceivePack => cmd_receive_pack(args),
    }
}

fn should_write_service_preamble(service: HttpBackendService) -> bool {
    !(service == HttpBackendService::UploadPack
        && env::var("HTTP_GIT_PROTOCOL")
            .ok()
            .is_some_and(|value| value.split(':').any(|token| token == "version=2")))
}

fn write_success_headers(content_type: &str) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write!(
        stdout,
        "Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\n\
         Pragma: no-cache\r\n\
         Cache-Control: no-cache, max-age=0, must-revalidate\r\n\
         Content-Type: {content_type}\r\n\r\n"
    )?;
    stdout.flush()?;
    Ok(())
}

fn cgi_error(status: u16, reason: &str, message: &str) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write!(
        stdout,
        "Status: {status} {reason}\r\n\
         Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\n\
         Pragma: no-cache\r\n\
         Cache-Control: no-cache, max-age=0, must-revalidate\r\n\r\n"
    )?;
    if !message.is_empty() {
        writeln!(stdout, "{message}")?;
    }
    stdout.flush()?;
    Ok(())
}
