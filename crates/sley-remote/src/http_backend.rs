//! Native smart-HTTP CGI request planning.
//!
//! The CGI wrapper owns environment and byte-stream I/O. This module owns the
//! stable routing rules which turn `PATH_INFO`/`QUERY_STRING` into an explicit
//! repository, service, and operation.

use std::path::{Component, Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, Result};

/// A smart-HTTP RPC service supported by Sley's native backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpBackendService {
    /// Fetch/clone service.
    UploadPack,
    /// Push service.
    ReceivePack,
}

impl HttpBackendService {
    /// Git's wire/CGI service name.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    /// CGI media type stem used for advertisements, requests, and results.
    pub const fn media_type_name(self) -> &'static str {
        match self {
            Self::UploadPack => "upload-pack",
            Self::ReceivePack => "receive-pack",
        }
    }
}

/// The smart-HTTP operation selected for a CGI request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpBackendOperation {
    /// `GET <repo>/info/refs?service=git-...`.
    Advertise,
    /// `POST <repo>/git-...`.
    Rpc,
}

/// Environment-derived input to [`plan_http_backend_request`].
#[derive(Clone, Copy, Debug)]
pub struct HttpBackendRequest<'a> {
    /// CGI request method (`GET`, `HEAD`, or `POST`).
    pub method: &'a str,
    /// CGI `PATH_INFO`.
    pub path_info: Option<&'a str>,
    /// CGI `PATH_TRANSLATED`, used when `GIT_PROJECT_ROOT` is absent.
    pub path_translated: Option<&'a Path>,
    /// Optional `GIT_PROJECT_ROOT`.
    pub project_root: Option<&'a Path>,
    /// CGI query string without the leading `?`.
    pub query_string: &'a str,
}

/// A validated native smart-HTTP operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpBackendPlan {
    /// Repository path passed to the owning engine command.
    pub repository: PathBuf,
    /// Selected smart service.
    pub service: HttpBackendService,
    /// Advertisement or RPC execution.
    pub operation: HttpBackendOperation,
    /// Whether a CGI `HEAD` request must suppress the response body.
    pub head_only: bool,
}

impl HttpBackendPlan {
    /// Response media type for this plan.
    pub fn response_content_type(&self) -> String {
        let suffix = match self.operation {
            HttpBackendOperation::Advertise => "advertisement",
            HttpBackendOperation::Rpc => "result",
        };
        format!(
            "application/x-git-{}-{suffix}",
            self.service.media_type_name()
        )
    }

    /// Required request media type for an RPC POST.
    pub fn request_content_type(&self) -> Option<String> {
        (self.operation == HttpBackendOperation::Rpc).then(|| {
            format!(
                "application/x-git-{}-request",
                self.service.media_type_name()
            )
        })
    }
}

/// Plan one CGI request using Git's smart-HTTP path conventions.
pub fn plan_http_backend_request(request: HttpBackendRequest<'_>) -> Result<HttpBackendPlan> {
    let translated = translated_request_path(&request)?;
    let (repository, service, operation) = if let Some(repository) =
        strip_path_suffix(&translated, Path::new("info/refs"))
    {
        let service = query_parameter(request.query_string, "service")
            .and_then(parse_service)
            .ok_or_else(|| GitError::Unsupported("smart HTTP service is missing".into()))?;
        (repository, service, HttpBackendOperation::Advertise)
    } else if let Some(repository) = strip_path_suffix(&translated, Path::new("git-upload-pack")) {
        (
            repository,
            HttpBackendService::UploadPack,
            HttpBackendOperation::Rpc,
        )
    } else if let Some(repository) = strip_path_suffix(&translated, Path::new("git-receive-pack")) {
        (
            repository,
            HttpBackendService::ReceivePack,
            HttpBackendOperation::Rpc,
        )
    } else {
        return Err(GitError::Unsupported(format!(
            "smart HTTP request path: {}",
            translated.display()
        )));
    };

    let head_only = request.method == "HEAD";
    let expected_method = match operation {
        HttpBackendOperation::Advertise => "GET",
        HttpBackendOperation::Rpc => "POST",
    };
    let effective_method = if head_only { "GET" } else { request.method };
    if effective_method != expected_method {
        return Err(GitError::InvalidFormat(format!(
            "smart HTTP {operation:?} requires {expected_method}, got {}",
            request.method
        )));
    }

    let repository = repository.to_path_buf();
    if repository.as_os_str().is_empty() {
        return Err(GitError::InvalidPath(
            "smart HTTP repository path is empty".into(),
        ));
    }
    Ok(HttpBackendPlan {
        repository,
        service,
        operation,
        head_only,
    })
}

fn strip_path_suffix<'a>(path: &'a Path, suffix: &Path) -> Option<&'a Path> {
    if !path.ends_with(suffix) {
        return None;
    }
    let mut prefix = path;
    for _ in suffix.components() {
        prefix = prefix.parent()?;
    }
    Some(prefix)
}

/// Apply Git's default `http.uploadpack`/`http.receivepack` policy.
pub fn http_backend_service_enabled(
    service: HttpBackendService,
    config: &GitConfig,
    authenticated: bool,
) -> bool {
    match service {
        HttpBackendService::UploadPack => {
            config.get_bool("http", None, "uploadpack").unwrap_or(true)
        }
        HttpBackendService::ReceivePack => config
            .get_bool("http", None, "receivepack")
            .unwrap_or(authenticated),
    }
}

fn translated_request_path(request: &HttpBackendRequest<'_>) -> Result<PathBuf> {
    if let Some(root) = request
        .project_root
        .filter(|root| !root.as_os_str().is_empty())
    {
        let path_info = request
            .path_info
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                GitError::InvalidPath("GIT_PROJECT_ROOT is set but PATH_INFO is not".into())
            })?;
        let relative = Path::new(path_info.trim_start_matches('/'));
        if relative
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
        {
            return Err(GitError::InvalidPath(format!(
                "aliased smart HTTP path: {path_info}"
            )));
        }
        return Ok(root.join(relative));
    }
    request
        .path_translated
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            GitError::InvalidPath("No GIT_PROJECT_ROOT or PATH_TRANSLATED from server".into())
        })
}

fn query_parameter<'a>(query: &'a str, wanted: &str) -> Option<&'a str> {
    query.split('&').rev().find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name == wanted).then_some(value)
    })
}

fn parse_service(value: &str) -> Option<HttpBackendService> {
    match value {
        "git-upload-pack" => Some(HttpBackendService::UploadPack),
        "git-receive-pack" => Some(HttpBackendService::ReceivePack),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_config::{ConfigEntry, ConfigSection};

    #[test]
    fn plans_receive_advertisement_below_project_root() {
        let plan = plan_http_backend_request(HttpBackendRequest {
            method: "GET",
            path_info: Some("/repo.git/info/refs"),
            path_translated: None,
            project_root: Some(Path::new("/srv/git")),
            query_string: "service=git-receive-pack",
        })
        .expect("valid smart HTTP advertisement");
        assert_eq!(plan.repository, Path::new("/srv/git/repo.git"));
        assert_eq!(plan.service, HttpBackendService::ReceivePack);
        assert_eq!(plan.operation, HttpBackendOperation::Advertise);
        assert_eq!(
            plan.response_content_type(),
            "application/x-git-receive-pack-advertisement"
        );
    }

    #[test]
    fn plans_upload_rpc_from_path_translated() {
        let plan = plan_http_backend_request(HttpBackendRequest {
            method: "POST",
            path_info: None,
            path_translated: Some(Path::new("/srv/git/repo.git/git-upload-pack")),
            project_root: None,
            query_string: "",
        })
        .expect("valid smart HTTP RPC");
        assert_eq!(plan.repository, Path::new("/srv/git/repo.git"));
        assert_eq!(plan.service, HttpBackendService::UploadPack);
        assert_eq!(plan.operation, HttpBackendOperation::Rpc);
    }

    #[test]
    fn rejects_aliased_project_root_path() {
        let error = plan_http_backend_request(HttpBackendRequest {
            method: "GET",
            path_info: Some("/../repo.git/info/refs"),
            path_translated: None,
            project_root: Some(Path::new("/srv/git")),
            query_string: "service=git-upload-pack",
        })
        .expect_err("parent traversal must be rejected");
        assert!(matches!(error, GitError::InvalidPath(_)));
    }

    #[test]
    fn receive_pack_defaults_to_authenticated_only() {
        let config = GitConfig::default();
        assert!(!http_backend_service_enabled(
            HttpBackendService::ReceivePack,
            &config,
            false
        ));
        assert!(http_backend_service_enabled(
            HttpBackendService::ReceivePack,
            &config,
            true
        ));
        let config = GitConfig {
            sections: vec![ConfigSection::new(
                "http",
                None,
                vec![ConfigEntry::new("receivepack", Some("true".into()))],
            )],
            ..GitConfig::default()
        };
        assert!(http_backend_service_enabled(
            HttpBackendService::ReceivePack,
            &config,
            false
        ));
    }
}
