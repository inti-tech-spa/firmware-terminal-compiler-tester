use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use samdebug_core::{
    CancellationToken, ErrorCategory, SamdebugError, SamdebugResult,
    ports::{DownloadReceipt, Downloader},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

const MAX_ARCHIVE_ENTRIES: usize = 200_000;
const MAX_EXPANDED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolManifest {
    pub schema_version: u32,
    pub channel: String,
    pub installable: bool,
    #[serde(default)]
    pub reason: Option<String>,
    pub required_tools: Vec<String>,
    pub artifacts: Vec<ToolArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolArtifact {
    pub name: String,
    pub version: String,
    pub os: String,
    pub architecture: String,
    pub url: String,
    pub allowed_hosts: Vec<String>,
    pub sha256: String,
    pub archive: ArchiveSpec,
    pub executables: Vec<ExecutableSpec>,
    pub licenses: Vec<LicenseSpec>,
    pub source_url: String,
    pub source_sha256: String,
    pub source_offer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ArchiveSpec {
    pub kind: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExecutableSpec {
    pub name: String,
    pub path: String,
    pub version_args: Vec<String>,
    pub version_contains: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LicenseSpec {
    pub spdx: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
}

impl Platform {
    #[must_use]
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallReport {
    pub root: PathBuf,
    pub installed: Vec<String>,
    pub reused: Vec<String>,
    pub offline: bool,
}

#[derive(Debug)]
pub struct Installer<'a> {
    root: PathBuf,
    platform: Platform,
    downloader: &'a dyn Downloader,
    cancellation: &'a CancellationToken,
}

impl<'a> Installer<'a> {
    #[must_use]
    pub fn new(
        root: PathBuf,
        platform: Platform,
        downloader: &'a dyn Downloader,
        cancellation: &'a CancellationToken,
    ) -> Self {
        Self {
            root,
            platform,
            downloader,
            cancellation,
        }
    }

    pub fn install(&self, manifest: &ToolManifest, offline: bool) -> SamdebugResult<InstallReport> {
        check_cancelled(self.cancellation)?;
        validate_manifest(manifest)?;
        if !manifest.installable {
            return Err(SamdebugError::new(
                ErrorCategory::Tool,
                "TOOL_MANIFEST_DISABLED",
                manifest
                    .reason
                    .as_deref()
                    .unwrap_or("the embedded tool manifest is not installable"),
            ));
        }
        fs::create_dir_all(self.root.join("downloads"))
            .and_then(|()| fs::create_dir_all(self.root.join("tools")))
            .map_err(|error| io_error("INSTALL_ROOT_FAILED", &error))?;
        self.remove_stale_staging()?;
        self.remove_stale_downloads()?;

        let selected: Vec<&ToolArtifact> = manifest
            .artifacts
            .iter()
            .filter(|artifact| {
                artifact.os == self.platform.os
                    && artifact.architecture == self.platform.architecture
            })
            .collect();
        let distinct: BTreeSet<&str> = selected
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect();
        let required_present = manifest
            .required_tools
            .iter()
            .all(|required| distinct.contains(required.as_str()));
        if distinct.len() != selected.len() || selected.is_empty() || !required_present {
            return Err(SamdebugError::new(
                ErrorCategory::Tool,
                "UNSUPPORTED_PLATFORM",
                format!(
                    "no unique tool set for {}-{}",
                    self.platform.os, self.platform.architecture
                ),
            ));
        }

        let mut installed = Vec::new();
        let mut reused = Vec::new();
        for artifact in selected {
            check_cancelled(self.cancellation)?;
            let key = format!("{}@{}", artifact.name, artifact.version);
            if self.install_one(artifact, offline)? {
                installed.push(key);
            } else {
                reused.push(key);
            }
        }
        Ok(InstallReport {
            root: self.root.clone(),
            installed,
            reused,
            offline,
        })
    }

    fn install_one(&self, artifact: &ToolArtifact, offline: bool) -> SamdebugResult<bool> {
        check_cancelled(self.cancellation)?;
        let destination = self
            .root
            .join("tools")
            .join(&artifact.name)
            .join(&artifact.version);
        if install_is_valid_with_cancellation(&destination, artifact, self.cancellation)? {
            validate_executable_versions(&destination, artifact, self.cancellation)?;
            return Ok(false);
        }
        if destination.exists() {
            fs::remove_dir_all(&destination)
                .map_err(|error| io_error("PARTIAL_INSTALL_CLEANUP_FAILED", &error))?;
        }

        let archive_path = self
            .root
            .join("downloads")
            .join(format!("{}-{}.tar.xz", artifact.name, artifact.sha256));
        let cache_valid = archive_path.exists()
            && sha256_file_with_cancellation(&archive_path, self.cancellation)? == artifact.sha256;
        if !cache_valid {
            if archive_path.exists() {
                fs::remove_file(&archive_path)
                    .map_err(|error| io_error("CORRUPT_CACHE_CLEANUP_FAILED", &error))?;
            }
            if offline {
                return Err(SamdebugError::new(
                    ErrorCategory::Tool,
                    "OFFLINE_CACHE_MISS",
                    format!("verified cache is unavailable for {}", artifact.name),
                ));
            }
            self.download_verified(artifact, &archive_path)?;
        }

        let parent = destination
            .parent()
            .expect("version destination has parent");
        fs::create_dir_all(parent).map_err(|error| io_error("INSTALL_PARENT_FAILED", &error))?;
        let staging = parent.join(format!(".staging-{}-{}", std::process::id(), nonce()?));
        fs::create_dir(&staging).map_err(|error| io_error("STAGING_CREATE_FAILED", &error))?;

        let result = (|| {
            extract_tar_xz(
                &archive_path,
                &staging,
                &artifact.archive,
                self.cancellation,
            )?;
            validate_staged(&staging, artifact, self.cancellation)?;
            validate_executable_versions(&staging, artifact, self.cancellation)?;
            let files = inventory_tree(&staging, self.cancellation)?;
            let marker = InstallMarker {
                schema_version: 2,
                name: artifact.name.clone(),
                version: artifact.version.clone(),
                archive_sha256: artifact.sha256.clone(),
                files,
            };
            let marker_bytes = serde_json::to_vec_pretty(&marker).expect("marker serializes");
            write_new(&staging.join("install.json"), &marker_bytes, 0o644)?;
            fs::rename(&staging, &destination)
                .map_err(|error| io_error("ATOMIC_INSTALL_FAILED", &error))?;
            Ok(())
        })();
        if result.is_err() && staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        result.map(|()| true)
    }

    fn download_verified(&self, artifact: &ToolArtifact, destination: &Path) -> SamdebugResult<()> {
        let partial = destination.with_extension(format!("partial-{}", nonce()?));
        let result = (|| {
            let receipt = self.downloader.download(
                &artifact.url,
                &artifact.allowed_hosts,
                &partial,
                self.cancellation,
            )?;
            validate_download_url(&receipt.final_url, &artifact.allowed_hosts)?;
            let actual = sha256_file_with_cancellation(&partial, self.cancellation)?;
            if actual != artifact.sha256 {
                return Err(SamdebugError::new(
                    ErrorCategory::Tool,
                    "TOOL_CHECKSUM_MISMATCH",
                    format!("expected {}, received {actual}", artifact.sha256),
                ));
            }
            fs::rename(&partial, destination)
                .map_err(|error| io_error("CACHE_PROMOTION_FAILED", &error))?;
            Ok(())
        })();
        if result.is_err() && partial.exists() {
            let _ = fs::remove_file(partial);
        }
        result
    }

    fn remove_stale_staging(&self) -> SamdebugResult<()> {
        let tools = self.root.join("tools");
        if !tools.exists() {
            return Ok(());
        }
        visit_directories(&tools, 2, &mut |path| {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".staging-"))
            {
                fs::remove_dir_all(path)
                    .map_err(|error| io_error("STALE_STAGING_CLEANUP_FAILED", &error))?;
            }
            Ok(())
        })
    }

    fn remove_stale_downloads(&self) -> SamdebugResult<()> {
        let downloads = self.root.join("downloads");
        for entry in fs::read_dir(downloads)
            .map_err(|error| io_error("DOWNLOAD_CACHE_SCAN_FAILED", &error))?
        {
            let path = entry
                .map_err(|error| io_error("DOWNLOAD_CACHE_SCAN_FAILED", &error))?
                .path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".partial-"))
            {
                fs::remove_file(path)
                    .map_err(|error| io_error("STALE_DOWNLOAD_CLEANUP_FAILED", &error))?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InstallMarker {
    schema_version: u32,
    name: String,
    version: String,
    archive_sha256: String,
    #[serde(default)]
    files: Vec<InstalledFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InstalledFile {
    path: String,
    size: u64,
    sha256: String,
}

pub(crate) fn validate_manifest(manifest: &ToolManifest) -> SamdebugResult<()> {
    if manifest.schema_version != 1
        || manifest.channel != "pinned"
        || manifest.required_tools.is_empty()
        || manifest.required_tools.iter().any(String::is_empty)
    {
        return Err(manifest_error("unsupported manifest schema or channel"));
    }
    for artifact in &manifest.artifacts {
        if artifact.name.is_empty()
            || artifact.version.is_empty()
            || artifact.allowed_hosts.is_empty()
            || artifact.executables.is_empty()
            || artifact.licenses.is_empty()
            || artifact.archive.kind != "tar.xz"
            || !is_sha256(&artifact.sha256)
            || !is_sha256(&artifact.source_sha256)
            || artifact.source_offer.is_empty()
        {
            return Err(manifest_error(
                "artifact has missing or invalid required metadata",
            ));
        }
        validate_download_url(&artifact.url, &artifact.allowed_hosts)?;
        validate_https_url(&artifact.source_url)?;
        validate_relative(&artifact.archive.root)?;
        for executable in &artifact.executables {
            validate_relative(&executable.path)?;
            if executable.name.is_empty() || executable.version_contains.is_empty() {
                return Err(manifest_error("invalid executable metadata"));
            }
        }
        for license in &artifact.licenses {
            validate_relative(&license.path)?;
            if license.spdx.is_empty() {
                return Err(manifest_error("license identifier is empty"));
            }
        }
    }
    Ok(())
}

fn validate_https_url(value: &str) -> SamdebugResult<Url> {
    let url = Url::parse(value).map_err(|_| manifest_error("invalid URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
    {
        return Err(manifest_error("URL must be credential-free HTTPS"));
    }
    Ok(url)
}

fn validate_download_url(value: &str, allowed_hosts: &[String]) -> SamdebugResult<()> {
    let url = validate_https_url(value)?;
    let host = url.host_str().expect("validated URL has host");
    if !allowed_hosts.iter().any(|allowed| allowed == host) {
        return Err(SamdebugError::new(
            ErrorCategory::Tool,
            "UNAPPROVED_DOWNLOAD_HOST",
            format!("download resolved to unapproved host {host}"),
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_relative(value: &str) -> SamdebugResult<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(manifest_error(
            "manifest path must contain only relative normal components",
        ));
    }
    Ok(())
}

fn extract_tar_xz(
    archive: &Path,
    staging: &Path,
    spec: &ArchiveSpec,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    extract_tar_xz_with_limit(archive, staging, spec, cancellation, MAX_EXPANDED_BYTES)
}

fn extract_tar_xz_with_limit(
    archive: &Path,
    staging: &Path,
    spec: &ArchiveSpec,
    cancellation: &CancellationToken,
    extracted_limit: u64,
) -> SamdebugResult<()> {
    check_cancelled(cancellation)?;
    let tar_path = staging.with_extension(format!("expanded-{}.tar", nonce()?));
    let result = (|| {
        decompress_xz(archive, &tar_path, cancellation)?;

        let mut tar = tar::Archive::new(
            File::open(&tar_path).map_err(|error| io_error("ARCHIVE_TEMP_FAILED", &error))?,
        );
        let entries = tar.entries().map_err(|error| archive_error(&error))?;
        let mut entry_count = 0usize;
        let mut expanded_bytes = 0u64;
        let mut pending_hard_links = Vec::new();
        for entry in entries {
            check_cancelled(cancellation)?;
            entry_count += 1;
            if entry_count > MAX_ARCHIVE_ENTRIES {
                return Err(archive_security_error("archive has too many entries"));
            }
            let mut entry = entry.map_err(|error| archive_error(&error))?;
            let entry_type = entry.header().entry_type();
            if entry_type.is_symlink()
                || (!entry_type.is_file() && !entry_type.is_dir() && !entry_type.is_hard_link())
            {
                return Err(archive_security_error(
                    "archive contains a link or special entry",
                ));
            }
            let raw_path = entry.path().map_err(|error| archive_error(&error))?;
            let relative = strip_archive_root(&raw_path, &spec.root)?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            let output = staging.join(&relative);
            if entry_type.is_hard_link() {
                let target = entry
                    .link_name()
                    .map_err(|error| archive_error(&error))?
                    .ok_or_else(|| archive_security_error("hard link has no target"))?;
                let target = strip_archive_root(&target, &spec.root)?;
                pending_hard_links.push((output, staging.join(target)));
                continue;
            }
            if entry_type.is_dir() {
                fs::create_dir_all(&output)
                    .map_err(|error| io_error("ARCHIVE_WRITE_FAILED", &error))?;
                continue;
            }
            let size = entry
                .header()
                .size()
                .map_err(|error| archive_error(&error))?;
            expanded_bytes = expanded_bytes
                .checked_add(size)
                .ok_or_else(|| archive_security_error("archive size overflow"))?;
            if expanded_bytes > extracted_limit {
                return Err(archive_security_error(
                    "archive expands beyond the configured limit",
                ));
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| io_error("ARCHIVE_WRITE_FAILED", &error))?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .map_err(|error| io_error("ARCHIVE_DUPLICATE_OR_WRITE_FAILED", &error))?;
            copy_cancellable(&mut entry, &mut file, cancellation)?;
            set_mode(&output, entry.header().mode().unwrap_or(0o644) & 0o777)?;
        }
        for (output, target) in pending_hard_links {
            check_cancelled(cancellation)?;
            let target_size = fs::symlink_metadata(&target)
                .map_err(|_| archive_security_error("hard-link target is missing"))?
                .len();
            expanded_bytes = expanded_bytes
                .checked_add(target_size)
                .ok_or_else(|| archive_security_error("archive size overflow"))?;
            if expanded_bytes > extracted_limit {
                return Err(archive_security_error(
                    "archive expands beyond the configured limit",
                ));
            }
            materialize_hard_link(&target, &output, cancellation)?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(tar_path);
    result
}

fn decompress_xz(
    archive: &Path,
    tar_path: &Path,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    let mut compressed = BufReader::new(
        File::open(archive).map_err(|error| io_error("ARCHIVE_OPEN_FAILED", &error))?,
    );
    let expanded_file = BufWriter::new(
        File::create(tar_path).map_err(|error| io_error("ARCHIVE_TEMP_FAILED", &error))?,
    );
    let mut expanded = LimitedWriter::new(expanded_file, MAX_EXPANDED_BYTES, cancellation);
    lzma_rs::xz_decompress(&mut compressed, &mut expanded).map_err(|error| {
        if cancellation.is_cancelled() {
            interrupted_error()
        } else {
            SamdebugError::new(
                ErrorCategory::Tool,
                "ARCHIVE_DECOMPRESSION_FAILED",
                error.to_string(),
            )
        }
    })?;
    expanded
        .flush()
        .map_err(|error| io_error("ARCHIVE_TEMP_FAILED", &error))
}

fn materialize_hard_link(
    target: &Path,
    output: &Path,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    let metadata = fs::symlink_metadata(target)
        .map_err(|_| archive_security_error("hard-link target is missing"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || output.exists() {
        return Err(archive_security_error("hard-link target is unsafe"));
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error("ARCHIVE_WRITE_FAILED", &error))?;
    }
    let mut source =
        File::open(target).map_err(|error| io_error("ARCHIVE_WRITE_FAILED", &error))?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| io_error("ARCHIVE_DUPLICATE_OR_WRITE_FAILED", &error))?;
    copy_cancellable(&mut source, &mut destination, cancellation)?;
    fs::set_permissions(output, metadata.permissions())
        .map_err(|error| io_error("FILE_MODE_FAILED", &error))
}

#[derive(Debug)]
struct LimitedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
    cancellation: CancellationToken,
}

impl<W> LimitedWriter<W> {
    fn new(inner: W, limit: u64, cancellation: &CancellationToken) -> Self {
        Self {
            inner,
            written: 0,
            limit,
            cancellation: cancellation.clone(),
        }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "operation interrupted",
            ));
        }
        let requested = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if self.written.saturating_add(requested) > self.limit {
            return Err(std::io::Error::other("expanded archive exceeds size limit"));
        }
        let count = self.inner.write(buffer)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn strip_archive_root(path: &Path, root: &str) -> SamdebugResult<PathBuf> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(archive_security_error(
            "archive path is absolute or traverses directories",
        ));
    }
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| archive_security_error("archive entry is outside the declared root"))
}

fn validate_staged(
    staging: &Path,
    artifact: &ToolArtifact,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    for executable in &artifact.executables {
        check_cancelled(cancellation)?;
        let path = staging.join(&executable.path);
        let metadata = fs::symlink_metadata(&path).map_err(|_| {
            SamdebugError::new(
                ErrorCategory::Tool,
                "EXECUTABLE_MISSING",
                format!("missing {}", executable.path),
            )
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(archive_security_error(
                "declared executable is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(SamdebugError::new(
                    ErrorCategory::Tool,
                    "EXECUTABLE_NOT_RUNNABLE",
                    &executable.path,
                ));
            }
        }
    }
    for license in &artifact.licenses {
        check_cancelled(cancellation)?;
        if !staging.join(&license.path).is_file() {
            return Err(SamdebugError::new(
                ErrorCategory::Tool,
                "LICENSE_FILE_MISSING",
                format!("missing {} ({})", license.path, license.spdx),
            ));
        }
    }
    Ok(())
}

fn validate_executable_versions(
    staging: &Path,
    artifact: &ToolArtifact,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    for executable in &artifact.executables {
        check_cancelled(cancellation)?;
        let mut command = Command::new(staging.join(&executable.path));
        command.args(&executable.version_args);
        let output = command_output_cancellable(&mut command, cancellation)?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() || !combined.contains(&executable.version_contains) {
            return Err(SamdebugError::new(
                ErrorCategory::Tool,
                "EXECUTABLE_VERSION_MISMATCH",
                format!(
                    "{} did not report {}",
                    executable.name, executable.version_contains
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn install_is_valid(
    destination: &Path,
    artifact: &ToolArtifact,
) -> SamdebugResult<bool> {
    install_is_valid_with_cancellation(destination, artifact, &CancellationToken::new())
}

fn install_is_valid_with_cancellation(
    destination: &Path,
    artifact: &ToolArtifact,
    cancellation: &CancellationToken,
) -> SamdebugResult<bool> {
    check_cancelled(cancellation)?;
    let marker_path = destination.join("install.json");
    if !marker_path.is_file() {
        return Ok(false);
    }
    let marker: InstallMarker = serde_json::from_reader(
        File::open(marker_path).map_err(|error| io_error("MARKER_READ_FAILED", &error))?,
    )
    .map_err(|error| {
        SamdebugError::new(ErrorCategory::Tool, "MARKER_INVALID", error.to_string())
    })?;
    if marker.schema_version != 2
        || marker.name != artifact.name
        || marker.version != artifact.version
        || marker.archive_sha256 != artifact.sha256
    {
        return Ok(false);
    }
    validate_staged(destination, artifact, cancellation)?;
    if inventory_tree(destination, cancellation)? != marker.files {
        return Ok(false);
    }
    Ok(true)
}

fn inventory_tree(
    root: &Path,
    cancellation: &CancellationToken,
) -> SamdebugResult<Vec<InstalledFile>> {
    let mut paths = Vec::new();
    collect_tree_paths(root, root, &mut paths, cancellation)?;
    paths.sort();
    let mut files = Vec::with_capacity(paths.len());
    for relative in paths {
        check_cancelled(cancellation)?;
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| io_error("INSTALL_INTEGRITY_SCAN_FAILED", &error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(archive_security_error(
                "installed tree contains an unsafe entry",
            ));
        }
        let path_text = relative
            .to_str()
            .ok_or_else(|| archive_security_error("installed path is not UTF-8"))?
            .to_owned();
        files.push(InstalledFile {
            path: path_text,
            size: metadata.len(),
            sha256: sha256_file_with_cancellation(&path, cancellation)?,
        });
    }
    Ok(files)
}

fn collect_tree_paths(
    root: &Path,
    current: &Path,
    paths: &mut Vec<PathBuf>,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    check_cancelled(cancellation)?;
    let mut entries = fs::read_dir(current)
        .map_err(|error| io_error("INSTALL_INTEGRITY_SCAN_FAILED", &error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io_error("INSTALL_INTEGRITY_SCAN_FAILED", &error))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        check_cancelled(cancellation)?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked path is below root")
            .to_path_buf();
        if relative == Path::new("install.json") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| io_error("INSTALL_INTEGRITY_SCAN_FAILED", &error))?;
        if metadata.file_type().is_symlink() {
            return Err(archive_security_error(
                "installed tree contains a symbolic link",
            ));
        }
        if metadata.is_dir() {
            collect_tree_paths(root, &path, paths, cancellation)?;
        } else if metadata.is_file() {
            paths.push(relative);
        } else {
            return Err(archive_security_error(
                "installed tree contains a special entry",
            ));
        }
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> SamdebugResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error("FILE_CREATE_FAILED", &error))?;
    file.write_all(bytes)
        .map_err(|error| io_error("FILE_WRITE_FAILED", &error))?;
    file.sync_all()
        .map_err(|error| io_error("FILE_SYNC_FAILED", &error))?;
    set_mode(path, mode)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> SamdebugResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| io_error("FILE_MODE_FAILED", &error))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> SamdebugResult<()> {
    Ok(())
}

pub(crate) fn sha256_file(path: &Path) -> SamdebugResult<String> {
    sha256_file_with_cancellation(path, &CancellationToken::new())
}

fn sha256_file_with_cancellation(
    path: &Path,
    cancellation: &CancellationToken,
) -> SamdebugResult<String> {
    let mut file = File::open(path).map_err(|error| io_error("HASH_OPEN_FAILED", &error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        check_cancelled(cancellation)?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| io_error("HASH_READ_FAILED", &error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn copy_cancellable(
    source: &mut dyn Read,
    destination: &mut dyn Write,
    cancellation: &CancellationToken,
) -> SamdebugResult<u64> {
    let mut copied = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        check_cancelled(cancellation)?;
        let count = source
            .read(&mut buffer)
            .map_err(|error| io_error("ARCHIVE_READ_FAILED", &error))?;
        if count == 0 {
            return Ok(copied);
        }
        destination
            .write_all(&buffer[..count])
            .map_err(|error| io_error("ARCHIVE_WRITE_FAILED", &error))?;
        copied = copied.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    }
}

fn command_output_cancellable(
    command: &mut Command,
    cancellation: &CancellationToken,
) -> SamdebugResult<Output> {
    check_cancelled(cancellation)?;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| io_error("EXECUTABLE_VERSION_FAILED", &error))?;
    loop {
        if cancellation.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(interrupted_error());
        }
        match child
            .try_wait()
            .map_err(|error| io_error("EXECUTABLE_VERSION_FAILED", &error))?
        {
            Some(_) => {
                return child
                    .wait_with_output()
                    .map_err(|error| io_error("EXECUTABLE_VERSION_FAILED", &error));
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> SamdebugResult<()> {
    if cancellation.is_cancelled() {
        Err(interrupted_error())
    } else {
        Ok(())
    }
}

fn interrupted_error() -> SamdebugError {
    SamdebugError::new(
        ErrorCategory::Interrupted,
        "INTERRUPTED",
        "operation interrupted",
    )
}

fn nonce() -> SamdebugResult<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| SamdebugError::new(ErrorCategory::Tool, "CLOCK_FAILED", error.to_string()))
}

fn visit_directories(
    root: &Path,
    depth: usize,
    callback: &mut dyn FnMut(&Path) -> SamdebugResult<()>,
) -> SamdebugResult<()> {
    if depth == 0 {
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|error| io_error("INSTALL_SCAN_FAILED", &error))? {
        let path = entry
            .map_err(|error| io_error("INSTALL_SCAN_FAILED", &error))?
            .path();
        if fs::symlink_metadata(&path)
            .map_err(|error| io_error("INSTALL_SCAN_FAILED", &error))?
            .is_dir()
        {
            callback(&path)?;
            if path.exists() {
                visit_directories(&path, depth - 1, callback)?;
            }
        }
    }
    Ok(())
}

fn manifest_error(message: &str) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Tool, "INVALID_TOOL_MANIFEST", message)
}

fn archive_error(error: &std::io::Error) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Tool, "ARCHIVE_INVALID", error.to_string())
}

fn archive_security_error(message: &str) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Tool, "UNSAFE_ARCHIVE", message)
}

fn io_error(code: &str, error: &std::io::Error) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Tool, code, error.to_string())
}

#[derive(Debug, Default)]
pub struct CurlDownloader;

impl Downloader for CurlDownloader {
    fn download(
        &self,
        url: &str,
        allowed_hosts: &[String],
        destination: &Path,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<DownloadReceipt> {
        validate_download_url(url, allowed_hosts)?;
        let mut command = Command::new("/usr/bin/curl");
        command
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-redirs",
                "5",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--output",
            ])
            .arg(destination)
            .args(["--write-out", "%{url_effective}", url]);
        let output = command_output_cancellable(&mut command, cancellation).map_err(|error| {
            if error.category() == ErrorCategory::Interrupted {
                error
            } else {
                SamdebugError::new(
                    ErrorCategory::Tool,
                    "DOWNLOAD_START_FAILED",
                    error.to_string(),
                )
            }
        })?;
        if !output.status.success() {
            return Err(SamdebugError::new(
                ErrorCategory::Tool,
                "DOWNLOAD_FAILED",
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        let final_url = String::from_utf8(output.stdout).map_err(|error| {
            SamdebugError::new(
                ErrorCategory::Tool,
                "DOWNLOAD_URL_INVALID",
                error.to_string(),
            )
        })?;
        validate_download_url(&final_url, allowed_hosts)?;
        Ok(DownloadReceipt { final_url })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Cursor,
        path::{Path, PathBuf},
        process::Command,
        sync::{Mutex, OnceLock},
    };

    use samdebug_core::{
        CancellationToken, SamdebugResult,
        ports::{DownloadReceipt, Downloader},
    };
    use tempfile::TempDir;

    use super::{
        ArchiveSpec, ExecutableSpec, InstallReport, Installer, LicenseSpec, Platform, ToolArtifact,
        ToolManifest, extract_tar_xz_with_limit, sha256_file,
    };

    #[derive(Debug)]
    struct FakeDownloader {
        source: PathBuf,
        final_url: String,
        calls: Mutex<usize>,
    }

    impl FakeDownloader {
        fn calls(&self) -> usize {
            *self.calls.lock().expect("lock")
        }
    }

    impl Downloader for FakeDownloader {
        fn download(
            &self,
            _url: &str,
            _allowed_hosts: &[String],
            destination: &Path,
            _cancellation: &CancellationToken,
        ) -> SamdebugResult<DownloadReceipt> {
            *self.calls.lock().expect("lock") += 1;
            fs::copy(&self.source, destination).expect("copy fixture archive");
            Ok(DownloadReceipt {
                final_url: self.final_url.clone(),
            })
        }
    }

    fn write_archive(path: &Path, hostile: Option<&str>, include_license: bool, version: &str) {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let program = format!("#!/bin/sh\necho tool {version}\n");
            let mut executable = tar::Header::new_gnu();
            executable.set_size(program.len() as u64);
            executable.set_mode(0o755);
            executable.set_entry_type(tar::EntryType::file());
            executable.set_cksum();
            builder
                .append_data(&mut executable, "bundle/bin/tool", program.as_bytes())
                .expect("append executable");

            if include_license {
                let license = b"fixture license";
                let mut header = tar::Header::new_gnu();
                header.set_size(license.len() as u64);
                header.set_mode(0o644);
                header.set_entry_type(tar::EntryType::file());
                header.set_cksum();
                builder
                    .append_data(&mut header, "bundle/LICENSE", &license[..])
                    .expect("append license");
            }

            if hostile == Some("symlink") {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o777);
                header.set_entry_type(tar::EntryType::symlink());
                header.set_link_name("../../escape").expect("link name");
                header.set_cksum();
                builder
                    .append_data(&mut header, "bundle/escape", Cursor::new([]))
                    .expect("append symlink");
            }
            if hostile == Some("traversal") {
                let bytes = b"escape";
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_entry_type(tar::EntryType::file());
                let name = b"bundle/../../escape";
                header.as_mut_bytes()[..name.len()].copy_from_slice(name);
                header.set_cksum();
                builder
                    .append(&header, &bytes[..])
                    .expect("append traversal");
            }
            builder.finish().expect("finish tar");
        }
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(tar_bytes), &mut compressed)
            .expect("compress fixture");
        fs::write(path, compressed).expect("write fixture");
    }

    fn write_hard_link_archive(path: &Path, targets: &[&str]) {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let bytes = vec![b'x'; 1024];
            let mut file = tar::Header::new_gnu();
            file.set_size(bytes.len() as u64);
            file.set_mode(0o644);
            file.set_entry_type(tar::EntryType::file());
            file.set_cksum();
            builder
                .append_data(&mut file, "bundle/data", bytes.as_slice())
                .expect("append hard-link target");
            for (index, target) in targets.iter().enumerate() {
                let mut link = tar::Header::new_gnu();
                link.set_size(0);
                link.set_mode(0o644);
                link.set_entry_type(tar::EntryType::hard_link());
                link.set_link_name(target).expect("hard-link target name");
                link.set_cksum();
                builder
                    .append_data(&mut link, format!("bundle/link-{index}"), Cursor::new([]))
                    .expect("append hard link");
            }
            builder.finish().expect("finish tar");
        }
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(tar_bytes), &mut compressed)
            .expect("compress fixture");
        fs::write(path, compressed).expect("write fixture");
    }

    fn artifact(archive: &Path) -> ToolArtifact {
        ToolArtifact {
            name: "fixture-tool".into(),
            version: "1.0".into(),
            os: "macos".into(),
            architecture: "aarch64".into(),
            url: "https://downloads.example.test/tool.tar.xz".into(),
            allowed_hosts: vec!["downloads.example.test".into()],
            sha256: sha256_file(archive).expect("hash fixture"),
            archive: ArchiveSpec {
                kind: "tar.xz".into(),
                root: "bundle".into(),
            },
            executables: vec![ExecutableSpec {
                name: "tool".into(),
                path: "bin/tool".into(),
                version_args: vec!["--version".into()],
                version_contains: "tool 1.0".into(),
            }],
            licenses: vec![LicenseSpec {
                spdx: "MIT".into(),
                path: "LICENSE".into(),
            }],
            source_url: "https://sources.example.test/tool.tar.xz".into(),
            source_sha256: "1".repeat(64),
            source_offer: "accompanying-source".into(),
        }
    }

    fn manifest(artifact: ToolArtifact) -> ToolManifest {
        ToolManifest {
            schema_version: 1,
            channel: "pinned".into(),
            installable: true,
            reason: None,
            required_tools: vec!["fixture-tool".into()],
            artifacts: vec![artifact],
        }
    }

    fn installer<'a>(root: &Path, downloader: &'a FakeDownloader) -> Installer<'a> {
        Installer::new(
            root.to_path_buf(),
            Platform {
                os: "macos".into(),
                architecture: "aarch64".into(),
            },
            downloader,
            test_token(),
        )
    }

    fn test_token() -> &'static CancellationToken {
        static TOKEN: OnceLock<CancellationToken> = OnceLock::new();
        TOKEN.get_or_init(CancellationToken::new)
    }

    fn setup_fixture(hostile: Option<&str>, license: bool, version: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let archive = temp.path().join("fixture.tar.xz");
        write_archive(&archive, hostile, license, version);
        (temp, archive)
    }

    #[test]
    fn clean_cached_and_offline_installations_are_atomic() {
        let (temp, archive) = setup_fixture(None, true, "1.0");
        let root = temp.path().join("managed");
        let downloader = FakeDownloader {
            source: archive.clone(),
            final_url: "https://downloads.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        let manifest = manifest(artifact(&archive));
        let first = installer(&root, &downloader)
            .install(&manifest, false)
            .expect("clean install");
        assert_eq!(first.installed, ["fixture-tool@1.0"]);
        assert_eq!(downloader.calls(), 1);

        let second = installer(&root, &downloader)
            .install(&manifest, true)
            .expect("installed reuse");
        assert_eq!(second.reused, ["fixture-tool@1.0"]);

        fs::remove_dir_all(root.join("tools/fixture-tool/1.0")).expect("remove installed copy");
        let offline = installer(&root, &downloader)
            .install(&manifest, true)
            .expect("offline cache reinstall");
        assert_eq!(offline.installed, ["fixture-tool@1.0"]);
        assert_eq!(downloader.calls(), 1);
    }

    #[test]
    fn checksum_failure_removes_partial_download() {
        let (temp, archive) = setup_fixture(None, true, "1.0");
        let root = temp.path().join("managed");
        let mut expected = artifact(&archive);
        expected.sha256 = "0".repeat(64);
        let downloader = FakeDownloader {
            source: archive,
            final_url: "https://downloads.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        let error = installer(&root, &downloader)
            .install(&manifest(expected), false)
            .expect_err("reject checksum");
        assert_eq!(error.code(), "TOOL_CHECKSUM_MISMATCH");
        let downloads = fs::read_dir(root.join("downloads")).expect("downloads exists");
        assert_eq!(downloads.count(), 0);
    }

    #[test]
    fn offline_missing_or_corrupt_cache_fails_closed() {
        let (temp, archive) = setup_fixture(None, true, "1.0");
        let root = temp.path().join("managed");
        let downloader = FakeDownloader {
            source: archive.clone(),
            final_url: "https://downloads.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        let artifact = artifact(&archive);
        let error = installer(&root, &downloader)
            .install(&manifest(artifact.clone()), true)
            .expect_err("cache miss");
        assert_eq!(error.code(), "OFFLINE_CACHE_MISS");

        let downloads = root.join("downloads");
        fs::create_dir_all(&downloads).expect("downloads");
        fs::write(
            downloads.join(format!("{}-{}.tar.xz", artifact.name, artifact.sha256)),
            b"corrupt",
        )
        .expect("corrupt cache");
        let error = installer(&root, &downloader)
            .install(&manifest(artifact), true)
            .expect_err("corrupt cache");
        assert_eq!(error.code(), "OFFLINE_CACHE_MISS");
    }

    #[test]
    fn hostile_links_and_traversal_are_rejected() {
        for hostile in ["symlink", "traversal"] {
            let (temp, archive) = setup_fixture(Some(hostile), true, "1.0");
            let root = temp.path().join("managed");
            let downloader = FakeDownloader {
                source: archive.clone(),
                final_url: "https://downloads.example.test/tool.tar.xz".into(),
                calls: Mutex::new(0),
            };
            let error = installer(&root, &downloader)
                .install(&manifest(artifact(&archive)), false)
                .expect_err("reject hostile archive");
            assert_eq!(error.code(), "UNSAFE_ARCHIVE");
            assert!(!temp.path().join("escape").exists());
        }
    }

    #[test]
    fn partial_install_and_stale_staging_recover() {
        let (temp, archive) = setup_fixture(None, true, "1.0");
        let root = temp.path().join("managed");
        fs::create_dir_all(root.join("tools/fixture-tool/1.0")).expect("partial destination");
        fs::create_dir_all(root.join("tools/fixture-tool/.staging-interrupted"))
            .expect("stale staging");
        fs::create_dir_all(root.join("downloads")).expect("downloads");
        fs::write(
            root.join("downloads/tool.tar.partial-interrupted"),
            b"partial",
        )
        .expect("stale partial download");
        let downloader = FakeDownloader {
            source: archive.clone(),
            final_url: "https://downloads.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        installer(&root, &downloader)
            .install(&manifest(artifact(&archive)), false)
            .expect("recover partial state");
        assert!(root.join("tools/fixture-tool/1.0/install.json").is_file());
        assert!(
            !root
                .join("tools/fixture-tool/.staging-interrupted")
                .exists()
        );
        assert!(!root.join("downloads/tool.tar.partial-interrupted").exists());
    }

    #[test]
    fn unsupported_platform_manifest_and_redirect_fail_closed() {
        let (temp, archive) = setup_fixture(None, true, "1.0");
        let root = temp.path().join("managed");
        let redirect = FakeDownloader {
            source: archive.clone(),
            final_url: "https://evil.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        let error = installer(&root, &redirect)
            .install(&manifest(artifact(&archive)), false)
            .expect_err("reject redirect");
        assert_eq!(error.code(), "UNAPPROVED_DOWNLOAD_HOST");

        let valid = FakeDownloader {
            source: archive.clone(),
            final_url: "https://downloads.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        let windows = Installer::new(
            root,
            Platform {
                os: "windows".into(),
                architecture: "x86_64".into(),
            },
            &valid,
            test_token(),
        );
        let error = windows
            .install(&manifest(artifact(&archive)), false)
            .expect_err("unsupported platform");
        assert_eq!(error.code(), "UNSUPPORTED_PLATFORM");
    }

    #[test]
    fn executable_version_and_license_are_validated() {
        for (license, version, expected) in [
            (false, "1.0", "LICENSE_FILE_MISSING"),
            (true, "wrong", "EXECUTABLE_VERSION_MISMATCH"),
        ] {
            let (temp, archive) = setup_fixture(None, license, version);
            let downloader = FakeDownloader {
                source: archive.clone(),
                final_url: "https://downloads.example.test/tool.tar.xz".into(),
                calls: Mutex::new(0),
            };
            let error = installer(&temp.path().join("managed"), &downloader)
                .install(&manifest(artifact(&archive)), false)
                .expect_err("reject invalid bundle");
            assert_eq!(error.code(), expected);
        }
    }

    #[test]
    fn hard_links_are_materialized_safely_and_charged_to_quota() {
        let temp = TempDir::new().expect("tempdir");
        let safe_archive = temp.path().join("safe-links.tar.xz");
        write_hard_link_archive(&safe_archive, &["bundle/data"]);
        let safe_stage = temp.path().join("safe-stage");
        fs::create_dir(&safe_stage).expect("safe staging");
        extract_tar_xz_with_limit(
            &safe_archive,
            &safe_stage,
            &ArchiveSpec {
                kind: "tar.xz".into(),
                root: "bundle".into(),
            },
            test_token(),
            2048,
        )
        .expect("one hard link fits quota");
        assert_eq!(
            fs::read(safe_stage.join("data")).expect("target"),
            vec![b'x'; 1024]
        );
        assert_eq!(
            fs::read(safe_stage.join("link-0")).expect("copy"),
            vec![b'x'; 1024]
        );

        let quota_stage = temp.path().join("quota-stage");
        fs::create_dir(&quota_stage).expect("quota staging");
        let error = extract_tar_xz_with_limit(
            &safe_archive,
            &quota_stage,
            &ArchiveSpec {
                kind: "tar.xz".into(),
                root: "bundle".into(),
            },
            test_token(),
            1024,
        )
        .expect_err("hard-link copy exceeds quota");
        assert_eq!(error.code(), "UNSAFE_ARCHIVE");

        let missing_archive = temp.path().join("missing-link.tar.xz");
        write_hard_link_archive(&missing_archive, &["bundle/missing"]);
        let missing_stage = temp.path().join("missing-stage");
        fs::create_dir(&missing_stage).expect("missing staging");
        let error = extract_tar_xz_with_limit(
            &missing_archive,
            &missing_stage,
            &ArchiveSpec {
                kind: "tar.xz".into(),
                root: "bundle".into(),
            },
            test_token(),
            4096,
        )
        .expect_err("missing hard-link target rejected");
        assert_eq!(error.code(), "UNSAFE_ARCHIVE");

        for (name, targets) in [
            ("external", vec!["outside/data"]),
            ("chained", vec!["bundle/link-1", "bundle/data"]),
        ] {
            let archive = temp.path().join(format!("{name}-link.tar.xz"));
            write_hard_link_archive(&archive, &targets);
            let stage = temp.path().join(format!("{name}-stage"));
            fs::create_dir(&stage).expect("link staging");
            let error = extract_tar_xz_with_limit(
                &archive,
                &stage,
                &ArchiveSpec {
                    kind: "tar.xz".into(),
                    root: "bundle".into(),
                },
                test_token(),
                8192,
            )
            .expect_err("unsafe hard link rejected");
            assert_eq!(error.code(), "UNSAFE_ARCHIVE");
        }
    }

    #[test]
    fn installed_tree_hashes_detect_tampering_and_restore_from_cache() {
        let (temp, archive) = setup_fixture(None, true, "1.0");
        let root = temp.path().join("managed");
        let downloader = FakeDownloader {
            source: archive.clone(),
            final_url: "https://downloads.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        let artifact = artifact(&archive);
        let manifest = manifest(artifact.clone());
        installer(&root, &downloader)
            .install(&manifest, false)
            .expect("initial install");
        let installed = root.join("tools/fixture-tool/1.0");
        fs::write(installed.join("LICENSE"), b"tampered").expect("tamper license");
        assert!(!super::install_is_valid(&installed, &artifact).expect("integrity result"));

        let repaired = installer(&root, &downloader)
            .install(&manifest, true)
            .expect("repair from verified cache");
        assert_eq!(repaired.installed, ["fixture-tool@1.0"]);
        assert_eq!(
            fs::read(installed.join("LICENSE")).expect("restored license"),
            b"fixture license"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn quarantine_metadata_is_not_propagated_from_archive() {
        let (temp, archive) = setup_fixture(None, true, "1.0");
        let set = Command::new("/usr/bin/xattr")
            .args(["-w", "com.apple.quarantine", "0081;fixture;samdebug;"])
            .arg(&archive)
            .status()
            .expect("set quarantine attribute");
        assert!(set.success());
        let root = temp.path().join("managed");
        let downloader = FakeDownloader {
            source: archive.clone(),
            final_url: "https://downloads.example.test/tool.tar.xz".into(),
            calls: Mutex::new(0),
        };
        let report: InstallReport = installer(&root, &downloader)
            .install(&manifest(artifact(&archive)), false)
            .expect("install quarantined archive");
        assert_eq!(report.installed.len(), 1);
        let read = Command::new("/usr/bin/xattr")
            .args(["-p", "com.apple.quarantine"])
            .arg(root.join("tools/fixture-tool/1.0/bin/tool"))
            .output()
            .expect("read quarantine attribute");
        assert!(!read.status.success(), "quarantine unexpectedly propagated");
    }

    #[test]
    #[ignore = "requires SAMDEBUG_ARM_ARCHIVE pointing to the official 15.2.Rel1 archive"]
    fn official_arm_bundle_installs_and_reports_versions() {
        let archive = PathBuf::from(
            std::env::var_os("SAMDEBUG_ARM_ARCHIVE").expect("SAMDEBUG_ARM_ARCHIVE is required"),
        );
        let production: ToolManifest =
            serde_json::from_str(include_str!("../../../tools/manifest-v1.json"))
                .expect("production manifest parses");
        let arm = production
            .artifacts
            .into_iter()
            .find(|artifact| artifact.name == "arm-gnu-toolchain")
            .expect("Arm artifact record");
        assert_eq!(sha256_file(&archive).expect("hash archive"), arm.sha256);
        let temp = TempDir::new().expect("tempdir");
        let downloader = FakeDownloader {
            source: archive,
            final_url: arm.url.clone(),
            calls: Mutex::new(0),
        };
        let report = installer(temp.path(), &downloader)
            .install(
                &ToolManifest {
                    schema_version: 1,
                    channel: "pinned".into(),
                    installable: true,
                    reason: None,
                    required_tools: vec!["arm-gnu-toolchain".into()],
                    artifacts: vec![arm],
                },
                false,
            )
            .expect("install official Arm bundle");
        assert_eq!(report.installed, ["arm-gnu-toolchain@15.2.Rel1"]);
    }

    #[test]
    #[ignore = "requires SAMDEBUG_OPENOCD_ARCHIVE pointing to the audited OpenOCD bundle"]
    fn project_openocd_bundle_installs_and_reports_version() {
        let archive = PathBuf::from(
            std::env::var_os("SAMDEBUG_OPENOCD_ARCHIVE")
                .expect("SAMDEBUG_OPENOCD_ARCHIVE is required"),
        );
        let production: ToolManifest =
            serde_json::from_str(include_str!("../../../tools/manifest-v1.json"))
                .expect("production manifest parses");
        let openocd = production
            .artifacts
            .into_iter()
            .find(|artifact| artifact.name == "openocd")
            .expect("OpenOCD artifact record");
        assert_eq!(sha256_file(&archive).expect("hash archive"), openocd.sha256);
        let temp = TempDir::new().expect("tempdir");
        let downloader = FakeDownloader {
            source: archive,
            final_url: openocd.url.clone(),
            calls: Mutex::new(0),
        };
        let report = installer(temp.path(), &downloader)
            .install(
                &ToolManifest {
                    schema_version: 1,
                    channel: "pinned".into(),
                    installable: true,
                    reason: None,
                    required_tools: vec!["openocd".into()],
                    artifacts: vec![openocd],
                },
                false,
            )
            .expect("install project OpenOCD bundle");
        assert_eq!(report.installed, ["openocd@0.12.0"]);
    }
}
