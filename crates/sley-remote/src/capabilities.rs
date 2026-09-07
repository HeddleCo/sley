/// Coarse transport classification for explicit URL queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTransportKind {
    Http,
    Ssh,
    Git,
    Local,
    Bundle,
}

impl RemoteTransportKind {
    pub fn from_url_scheme(url: &str) -> Option<Self> {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(Self::Http);
        }
        if url.starts_with("git://") {
            return Some(Self::Git);
        }
        if url.starts_with("ssh://")
            || url.starts_with("git@")
            || url.contains(':') && !url.contains("://")
        {
            return Some(Self::Ssh);
        }
        if url.starts_with("file://") || url.starts_with('/') || url.starts_with("./") {
            return Some(Self::Local);
        }
        if url.ends_with(".bundle") {
            return Some(Self::Bundle);
        }
        None
    }
}
