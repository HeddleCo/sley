//! Signing and verification helpers shared by commit, tag, log, and verify-*
//! commands.

use crate::*;
use std::process::Stdio;
use sley::plumbing::{sley_config};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SigningFormat {
    OpenPgp,
    Ssh,
    X509,
}

pub(crate) fn signing_format(config: Option<&GitConfig>) -> Result<SigningFormat> {
    match config.and_then(|config| config.get("gpg", None, "format")) {
        None | Some("openpgp") => Ok(SigningFormat::OpenPgp),
        Some("ssh") => Ok(SigningFormat::Ssh),
        Some("x509") => Ok(SigningFormat::X509),
        Some(value) => {
            eprintln!("fatal: unsupported value for gpg.format: {value}");
            Err(GitError::Exit(128))
        }
    }
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
    match signing_format(config)? {
        SigningFormat::OpenPgp => sign_gpg_payload(gpg_program(config), payload, key, true),
        SigningFormat::Ssh => sign_ssh_payload(config, payload, key),
        SigningFormat::X509 => sign_gpg_payload(gpg_x509_program(config), payload, key, false),
    }
}

fn sign_gpg_payload(
    program: PathBuf,
    payload: &[u8],
    key: Option<&str>,
    use_long_keyid: bool,
) -> Result<Vec<u8>> {
    let mut command = ProcessCommand::new(program);
    command.arg("--status-fd=2");
    if use_long_keyid {
        command.arg("--keyid-format=long");
    }
    command.arg("-bsau");
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
    match verification_format(config, signature)? {
        SigningFormat::OpenPgp => verify_gpg_payload(
            gpg_program(config),
            git_dir,
            config,
            payload,
            signature,
            true,
        ),
        SigningFormat::Ssh => verify_ssh_payload(git_dir, config, payload, signature),
        SigningFormat::X509 => verify_gpg_payload(
            gpg_x509_program(config),
            git_dir,
            config,
            payload,
            signature,
            false,
        ),
    }
}

fn verification_format(config: Option<&GitConfig>, signature: &[u8]) -> Result<SigningFormat> {
    if signature
        .split(|byte| *byte == b'\n')
        .any(|line| line == b"-----BEGIN SSH SIGNATURE-----")
    {
        return Ok(SigningFormat::Ssh);
    }
    signing_format(config)
}

fn verify_gpg_payload(
    program: PathBuf,
    git_dir: &Path,
    config: Option<&GitConfig>,
    payload: &[u8],
    signature: &[u8],
    use_long_keyid: bool,
) -> Result<GpgVerification> {
    let temp = GpgTempFiles::new(git_dir)?;
    fs::write(&temp.payload, payload)?;
    fs::write(&temp.signature, signature)?;
    let mut command = ProcessCommand::new(program);
    command.arg("--status-fd=1");
    if use_long_keyid {
        command.arg("--keyid-format=long");
    }
    let output = command
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
    let Some(header_end) = body
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|idx| idx + 1)
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

pub(crate) fn signature_has_marker(signature: &[u8]) -> bool {
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

fn gpg_x509_program(config: Option<&GitConfig>) -> PathBuf {
    config
        .and_then(|config| config.get("gpg", Some("x509"), "program"))
        .map(sley_config::expand_user_path)
        .unwrap_or_else(|| PathBuf::from("gpgsm"))
}

fn ssh_program(config: Option<&GitConfig>) -> PathBuf {
    config
        .and_then(|config| config.get("gpg", Some("ssh"), "program"))
        .map(sley_config::expand_user_path)
        .unwrap_or_else(|| PathBuf::from("ssh-keygen"))
}

fn ssh_allowed_signers_file(config: Option<&GitConfig>) -> Option<PathBuf> {
    config
        .and_then(|config| config.get("gpg", Some("ssh"), "allowedSignersFile"))
        .filter(|value| !value.is_empty())
        .map(sley_config::expand_user_path)
}

fn sign_ssh_payload(
    config: Option<&GitConfig>,
    payload: &[u8],
    key: Option<&str>,
) -> Result<Vec<u8>> {
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        eprintln!("error: user.signingKey needs to be set for ssh signing");
        return Err(GitError::Exit(128));
    };
    let temp = GpgTempFiles::new(Path::new(".git"))?;
    let (key_path, use_agent) = ssh_signing_key_file(&temp, key)?;
    fs::write(&temp.payload, payload)?;
    let mut command = ProcessCommand::new(ssh_program(config));
    command
        .arg("-Y")
        .arg("sign")
        .arg("-n")
        .arg("git")
        .arg("-f")
        .arg(key_path);
    if use_agent {
        command.arg("-U");
    }
    let output = command
        .arg(&temp.payload)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| GitError::Command(format!("could not run ssh-keygen: {err}")))?;
    if !output.status.success() {
        io::stderr().write_all(&output.stderr)?;
        eprintln!("error: ssh-keygen failed to sign the data");
        return Err(GitError::Exit(128));
    }
    let signature = fs::read(&temp.ssh_signature).map_err(|err| {
        GitError::Command(format!(
            "failed reading ssh signing data buffer from '{}': {err}",
            temp.ssh_signature.display()
        ))
    })?;
    if !signature_has_marker(&signature) {
        io::stderr().write_all(&output.stderr)?;
        eprintln!("error: ssh-keygen failed to sign the data");
        return Err(GitError::Exit(128));
    }
    Ok(strip_cr(signature))
}

fn ssh_signing_key_file<'a>(temp: &'a GpgTempFiles, key: &'a str) -> Result<(&'a Path, bool)> {
    let Some(literal) = literal_ssh_key(key) else {
        return Ok((Path::new(key), false));
    };
    fs::write(&temp.key, literal)?;
    Ok((&temp.key, true))
}

fn literal_ssh_key(value: &str) -> Option<&str> {
    value
        .strip_prefix("key::")
        .or_else(|| value.starts_with("ssh-").then_some(value))
        .or_else(|| value.starts_with("ecdsa-").then_some(value))
        .or_else(|| value.starts_with("sk-").then_some(value))
}

fn verify_ssh_payload(
    git_dir: &Path,
    config: Option<&GitConfig>,
    payload: &[u8],
    signature: &[u8],
) -> Result<GpgVerification> {
    let Some(allowed_signers) = ssh_allowed_signers_file(config) else {
        return Ok(ssh_error_verification(
            b"error: gpg.ssh.allowedSignersFile needs to be configured and exist for ssh signature verification\n",
        ));
    };
    if !allowed_signers.is_file() {
        return Ok(ssh_error_verification(
            format!(
                "error: gpg.ssh.allowedSignersFile needs to be configured and exist for ssh signature verification\n"
            )
            .as_bytes(),
        ));
    }
    let temp = GpgTempFiles::new(git_dir)?;
    fs::write(&temp.signature, signature)?;
    let verify_time = ssh_verify_time_arg(payload);
    let program = ssh_program(config);
    let find = ssh_find_principals(
        &program,
        &allowed_signers,
        &temp.signature,
        verify_time.as_deref(),
    )?;
    let mut human_output = Vec::new();
    let mut ret_success = false;
    if find.status.success() && !find.stdout.is_empty() {
        for principal in String::from_utf8_lossy(&find.stdout)
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
        {
            let output = ssh_verify_principal(
                &program,
                &allowed_signers,
                &temp.signature,
                verify_time.as_deref(),
                principal,
                payload,
            )?;
            let good_output = output.stdout;
            human_output = good_output.clone();
            human_output.extend_from_slice(&find.stderr);
            human_output.extend_from_slice(&output.stderr);
            ret_success = output.status.success() && good_output.starts_with(b"Good");
            if ret_success {
                break;
            }
        }
    } else {
        let output =
            ssh_check_novalidate(&program, &temp.signature, verify_time.as_deref(), payload)?;
        let good_output = output.stdout;
        human_output = good_output.clone();
        human_output.extend_from_slice(&find.stderr);
        human_output.extend_from_slice(&output.stderr);
    }
    let mut verification = parse_ssh_output(&human_output);
    verification.human_output = human_output;
    verification.status_output = verification.human_output.clone();
    verification.success = ret_success && min_trust_satisfied(config, &verification);
    Ok(verification)
}

fn ssh_find_principals(
    program: &Path,
    allowed_signers: &Path,
    signature: &Path,
    verify_time: Option<&str>,
) -> Result<std::process::Output> {
    let mut command = ProcessCommand::new(program);
    command
        .arg("-Y")
        .arg("find-principals")
        .arg("-f")
        .arg(allowed_signers)
        .arg("-s")
        .arg(signature);
    if let Some(verify_time) = verify_time {
        command.arg(verify_time);
    }
    command
        .output()
        .map_err(|err| GitError::Command(format!("could not run ssh-keygen: {err}")))
}

fn ssh_verify_principal(
    program: &Path,
    allowed_signers: &Path,
    signature: &Path,
    verify_time: Option<&str>,
    principal: &str,
    payload: &[u8],
) -> Result<std::process::Output> {
    let mut command = ProcessCommand::new(program);
    command
        .arg("-Y")
        .arg("verify")
        .arg("-n")
        .arg("git")
        .arg("-f")
        .arg(allowed_signers)
        .arg("-I")
        .arg(principal)
        .arg("-s")
        .arg(signature);
    if let Some(verify_time) = verify_time {
        command.arg(verify_time);
    }
    command_with_stdin(command, payload)
        .map_err(|err| GitError::Command(format!("could not run ssh-keygen: {err}")))
}

fn ssh_check_novalidate(
    program: &Path,
    signature: &Path,
    verify_time: Option<&str>,
    payload: &[u8],
) -> Result<std::process::Output> {
    let mut command = ProcessCommand::new(program);
    command
        .arg("-Y")
        .arg("check-novalidate")
        .arg("-n")
        .arg("git")
        .arg("-s")
        .arg(signature);
    if let Some(verify_time) = verify_time {
        command.arg(verify_time);
    }
    command_with_stdin(command, payload)
        .map_err(|err| GitError::Command(format!("could not run ssh-keygen: {err}")))
}

fn command_with_stdin(
    mut command: ProcessCommand,
    input: &[u8],
) -> std::io::Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "could not open stdin"))?
        .write_all(input)?;
    child.wait_with_output()
}

fn parse_ssh_output(output: &[u8]) -> GpgVerification {
    let mut out = GpgVerification {
        trust: "never".to_string(),
        status: GpgSignatureStatus::Bad,
        ..GpgVerification::default()
    };
    let first = String::from_utf8_lossy(output)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let Some(rest) = first.strip_prefix("Good \"git\" signature ") else {
        return out;
    };
    let key_part;
    if let Some(rest) = rest.strip_prefix("for ") {
        let Some((principal, with_key)) = rest.rsplit_once(" with ") else {
            return out;
        };
        out.status = GpgSignatureStatus::Good;
        out.trust = "fully".to_string();
        out.signer = principal.to_string();
        key_part = with_key;
    } else if let Some(rest) = rest.strip_prefix("with ") {
        out.status = GpgSignatureStatus::Good;
        out.trust = "undefined".to_string();
        key_part = rest;
    } else {
        return out;
    }
    if let Some((_, fingerprint)) = key_part.split_once("key ") {
        out.key = fingerprint.to_string();
        out.fingerprint = fingerprint.to_string();
    } else {
        out.status = GpgSignatureStatus::Bad;
        out.trust = "never".to_string();
        out.signer.clear();
    }
    out
}

fn ssh_error_verification(message: &[u8]) -> GpgVerification {
    GpgVerification {
        status: GpgSignatureStatus::Unknown,
        trust: "undefined".to_string(),
        human_output: message.to_vec(),
        status_output: message.to_vec(),
        ..GpgVerification::default()
    }
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
    trust_rank(&verification.trust) >= trust_rank(&min.to_ascii_lowercase())
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

fn strip_cr(bytes: Vec<u8>) -> Vec<u8> {
    bytes.into_iter().filter(|byte| *byte != b'\r').collect()
}

fn ssh_verify_time_arg(payload: &[u8]) -> Option<String> {
    let timestamp = payload_identity_timestamp(payload)?;
    let (year, month, day, hour, minute, second) = utc_ymdhms(timestamp)?;
    Some(format!(
        "-Overify-time={year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}"
    ))
}

fn payload_identity_timestamp(payload: &[u8]) -> Option<i64> {
    for line in payload.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            break;
        }
        let value = line
            .strip_prefix(b"committer ")
            .or_else(|| line.strip_prefix(b"tagger "));
        if let Some(value) = value {
            return identity_timestamp(value);
        }
    }
    None
}

fn identity_timestamp(identity: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(identity).ok()?;
    let (before_tz, _tz) = text.rsplit_once(' ')?;
    let (_name_email, timestamp) = before_tz.rsplit_once(' ')?;
    timestamp.parse::<i64>().ok()
}

fn utc_ymdhms(timestamp: i64) -> Option<(i64, u32, u32, u32, u32, u32)> {
    if timestamp < 0 {
        return None;
    }
    let days = timestamp / 86_400;
    let seconds_of_day = timestamp % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = (seconds_of_day / 3_600) as u32;
    let minute = ((seconds_of_day % 3_600) / 60) as u32;
    let second = (seconds_of_day % 60) as u32;
    Some((year, month, day, hour, minute, second))
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
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
        if let Some(value) =
            header_value(content, b"gpgsig").or_else(|| header_value(content, b"gpgsig-sha256"))
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
    ssh_signature: PathBuf,
    key: PathBuf,
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
        let payload = dir.join(format!("{id}.payload"));
        let mut ssh_signature = payload.as_os_str().to_os_string();
        ssh_signature.push(".sig");
        let ssh_signature = PathBuf::from(ssh_signature);
        Ok(Self {
            payload,
            signature: dir.join(format!("{id}.sig")),
            ssh_signature,
            key: dir.join(format!("{id}.key")),
        })
    }
}

impl Drop for GpgTempFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.payload);
        let _ = fs::remove_file(&self.signature);
        let _ = fs::remove_file(&self.ssh_signature);
        let _ = fs::remove_file(&self.key);
    }
}
