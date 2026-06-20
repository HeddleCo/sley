//! OpenPGP signing and verification helpers shared by commit, tag, log, and
//! verify-* commands.

use crate::*;
use std::process::Stdio;

#[derive(Debug, Clone, Default)]
pub(crate) struct GpgVerification {
    pub success: bool,
    pub status: GpgSignatureStatus,
    pub trust: String,
    pub key: String,
    pub signer: String,
    pub fingerprint: String,
    pub primary_fingerprint: String,
    pub status_output: Vec<u8>,
    pub human_output: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum GpgSignatureStatus {
    Good,
    Bad,
    Unknown,
    #[default]
    None,
}

impl GpgVerification {
    pub(crate) fn pretty_code(&self) -> u8 {
        match self.status {
            GpgSignatureStatus::Good if self.trust == "undefined" || self.trust == "never" => b'U',
            GpgSignatureStatus::Good => b'G',
            GpgSignatureStatus::Bad => b'B',
            GpgSignatureStatus::Unknown => b'E',
            GpgSignatureStatus::None => b'N',
        }
    }
}

pub(crate) fn validate_openpgp_format(config: Option<&GitConfig>) -> Result<()> {
    if let Some(value) = config.and_then(|config| config.get("gpg", None, "format"))
        && value != "openpgp"
    {
        eprintln!("fatal: unsupported value for gpg.format: {value}");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub(crate) fn committer_email(identity: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(identity);
    let (_, rest) = text.rsplit_once('<')?;
    let (email, _) = rest.split_once('>')?;
    (!email.is_empty()).then(|| email.to_string())
}

pub(crate) fn signing_key(
    config: Option<&GitConfig>,
    explicit: Option<&str>,
    default_identity: &[u8],
) -> Option<String> {
    explicit
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            config
                .and_then(|config| config.get("user", None, "signingkey"))
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .or_else(|| committer_email(default_identity))
}

pub(crate) fn sign_payload(
    config: Option<&GitConfig>,
    payload: &[u8],
    key: Option<&str>,
) -> Result<Vec<u8>> {
    validate_openpgp_format(config)?;
    let program = gpg_program(config);
    let mut command = ProcessCommand::new(program);
    command
        .arg("--status-fd=2")
        .arg("--keyid-format=long")
        .arg("-bsau");
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        command.arg(key);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(format!("could not run gpg: {err}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| GitError::Command("could not open gpg stdin".into()))?
        .write_all(payload)?;
    let output = child.wait_with_output()?;
    if !output.status.success() || !signature_has_marker(&output.stdout) {
        io::stderr().write_all(&output.stderr)?;
        eprintln!("error: gpg failed to sign the data");
        return Err(GitError::Exit(128));
    }
    Ok(output.stdout)
}

pub(crate) fn verify_payload(
    git_dir: &Path,
    config: Option<&GitConfig>,
    payload: &[u8],
    signature: &[u8],
) -> Result<GpgVerification> {
    validate_openpgp_format(config)?;
    let temp = GpgTempFiles::new(git_dir)?;
    fs::write(&temp.payload, payload)?;
    fs::write(&temp.signature, signature)?;
    let output = ProcessCommand::new(gpg_program(config))
        .arg("--status-fd=1")
        .arg("--keyid-format=long")
        .arg("--verify")
        .arg(&temp.signature)
        .arg(&temp.payload)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| GitError::Command(format!("could not run gpg: {err}")))?;
    let mut verification = parse_gpg_status(&output.stdout);
    verification.human_output = output.stderr;
    verification.status_output = output.stdout;
    verification.success = output.status.success() && min_trust_satisfied(config, &verification);
    Ok(verification)
}

pub(crate) fn commit_signature_payload(body: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let signature = extract_commit_signature(body)?;
    let payload = commit_payload_without_signature(body);
    Some((payload, signature))
}

pub(crate) fn commit_payload_without_signature(body: &[u8]) -> Vec<u8> {
    let Some(header_end) = body.windows(2).position(|window| window == b"\n\n").map(|idx| idx + 1)
    else {
        return body.to_vec();
    };
    let header = &body[..header_end];
    let rest = &body[header_end..];
    let mut out = Vec::with_capacity(body.len());
    let mut skipping = false;
    for line in header.split_inclusive(|byte| *byte == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        if skipping && content.first() == Some(&b' ') {
            continue;
        }
        skipping = false;
        if header_line_has_key(content, b"gpgsig") || header_line_has_key(content, b"gpgsig-sha256")
        {
            skipping = true;
            continue;
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(rest);
    out
}

pub(crate) fn tag_signature_payload(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let start = tag_signature_offset(body)?;
    Some((&body[..start], &body[start..]))
}

pub(crate) fn tag_signature_offset(body: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    let mut found = None;
    while line_start < body.len() {
        let line_end = body[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(body.len(), |idx| line_start + idx);
        let line = &body[line_start..line_end];
        if TAG_SIGNATURE_MARKERS.contains(&line) {
            found = Some(line_start);
        }
        line_start = if line_end < body.len() {
            line_end + 1
        } else {
            body.len()
        };
    }
    found
}

pub(crate) fn bare_signature_output(verification: &GpgVerification) -> Vec<u8> {
    verification.human_output.clone()
}

const TAG_SIGNATURE_MARKERS: [&[u8]; 4] = [
    b"-----BEGIN PGP SIGNATURE-----",
    b"-----BEGIN PGP MESSAGE-----",
    b"-----BEGIN SIGNED MESSAGE-----",
    b"-----BEGIN SSH SIGNATURE-----",
];

fn signature_has_marker(signature: &[u8]) -> bool {
    signature
        .split(|byte| *byte == b'\n')
        .any(|line| TAG_SIGNATURE_MARKERS.contains(&line))
}

fn gpg_program(config: Option<&GitConfig>) -> PathBuf {
    config
        .and_then(|config| config.get("gpg", None, "program"))
        .map(sley_config::expand_user_path)
        .unwrap_or_else(|| PathBuf::from("gpg"))
}

fn parse_gpg_status(status: &[u8]) -> GpgVerification {
    let mut out = GpgVerification {
        trust: "undefined".to_string(),
        ..GpgVerification::default()
    };
    let mut saw_signature = false;
    let mut ambiguous = false;
    for line in String::from_utf8_lossy(status).lines() {
        let Some(rest) = line.strip_prefix("[GNUPG:] ") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let Some(kind) = fields.next() else {
            continue;
        };
        match kind {
            "GOODSIG" => {
                if saw_signature {
                    ambiguous = true;
                    continue;
                }
                saw_signature = true;
                out.status = GpgSignatureStatus::Good;
                out.key = fields.next().unwrap_or_default().to_string();
                out.signer = fields.collect::<Vec<_>>().join(" ");
            }
            "BADSIG" => {
                if saw_signature {
                    ambiguous = true;
                    continue;
                }
                saw_signature = true;
                out.status = GpgSignatureStatus::Bad;
                out.key = fields.next().unwrap_or_default().to_string();
                out.signer = fields.collect::<Vec<_>>().join(" ");
            }
            "ERRSIG" => {
                if out.status == GpgSignatureStatus::None {
                    out.status = GpgSignatureStatus::Unknown;
                }
                out.key = fields.next().unwrap_or_default().to_string();
            }
            "NO_PUBKEY" => {
                if out.status == GpgSignatureStatus::None {
                    out.status = GpgSignatureStatus::Unknown;
                }
                if out.key.is_empty() {
                    out.key = fields.next().unwrap_or_default().to_string();
                }
            }
            "VALIDSIG" => {
                let values = fields.collect::<Vec<_>>();
                if let Some(fingerprint) = values.first() {
                    out.fingerprint = (*fingerprint).to_string();
                }
                if let Some(primary) = values.last()
                    && primary.len() >= 16
                {
                    out.primary_fingerprint = (*primary).to_string();
                }
            }
            "TRUST_UNDEFINED" => out.trust = "undefined".to_string(),
            "TRUST_NEVER" => out.trust = "never".to_string(),
            "TRUST_MARGINAL" => out.trust = "marginal".to_string(),
            "TRUST_FULLY" => out.trust = "fully".to_string(),
            "TRUST_ULTIMATE" => out.trust = "ultimate".to_string(),
            _ => {}
        }
    }
    if ambiguous {
        out.status = GpgSignatureStatus::Unknown;
        out.trust.clear();
        out.key.clear();
        out.signer.clear();
        out.fingerprint.clear();
        out.primary_fingerprint.clear();
    }
    out
}

fn min_trust_satisfied(config: Option<&GitConfig>, verification: &GpgVerification) -> bool {
    let Some(min) = config.and_then(|config| config.get("gpg", None, "minTrustLevel")) else {
        return true;
    };
    trust_rank(&verification.trust) >= trust_rank(min)
}

fn trust_rank(value: &str) -> u8 {
    match value {
        "ultimate" => 4,
        "fully" => 3,
        "marginal" => 2,
        "never" => 1,
        _ => 0,
    }
}

fn extract_commit_signature(body: &[u8]) -> Option<Vec<u8>> {
    let header_end = body.windows(2).position(|window| window == b"\n\n")?;
    let header = &body[..header_end + 1];
    let mut signature = Vec::new();
    let mut collecting = false;
    for line in header.split_inclusive(|byte| *byte == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        if collecting && content.first() == Some(&b' ') {
            signature.extend_from_slice(&content[1..]);
            signature.push(b'\n');
            continue;
        }
        if collecting {
            break;
        }
        if let Some(value) = header_value(content, b"gpgsig")
            .or_else(|| header_value(content, b"gpgsig-sha256"))
        {
            collecting = true;
            signature.extend_from_slice(value);
            signature.push(b'\n');
        }
    }
    (!signature.is_empty()).then_some(signature)
}

fn header_line_has_key(line: &[u8], key: &[u8]) -> bool {
    line.len() > key.len() && line.starts_with(key) && line[key.len()] == b' '
}

fn header_value<'a>(line: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    header_line_has_key(line, key).then(|| &line[key.len() + 1..])
}

struct GpgTempFiles {
    payload: PathBuf,
    signature: PathBuf,
}

impl GpgTempFiles {
    fn new(git_dir: &Path) -> Result<Self> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        );
        let dir = git_dir.join("sley-gpg");
        fs::create_dir_all(&dir)?;
        Ok(Self {
            payload: dir.join(format!("{id}.payload")),
            signature: dir.join(format!("{id}.sig")),
        })
    }
}

impl Drop for GpgTempFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.payload);
        let _ = fs::remove_file(&self.signature);
    }
}
