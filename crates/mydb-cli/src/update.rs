use anyhow::{bail, Context, Result};
use clap::Args;
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const REPOSITORY: &str = "neko233-com/mydb";

#[derive(Args, Debug, Clone)]
pub struct UpdateOptions {
    /// Release tag to install, or `latest` for the newest stable release.
    #[arg(long, default_value = "latest")]
    pub version: String,

    /// Download and verify the package without changing installed binaries.
    #[arg(long)]
    pub check: bool,

    /// Directory containing the installed MyDB binaries.
    #[arg(long)]
    pub install_dir: Option<PathBuf>,

    /// Service name to stop and restart while updating the server binary.
    #[arg(long)]
    pub service_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReleaseAsset {
    archive_name: &'static str,
    installer_name: &'static str,
    kind: ArchiveKind,
}

pub fn run(options: &UpdateOptions) -> Result<()> {
    let asset = release_asset()?;
    let tag = normalize_tag(&options.version)?;
    let release_base = if tag == "latest" {
        format!("https://github.com/{REPOSITORY}/releases/latest/download")
    } else {
        format!("https://github.com/{REPOSITORY}/releases/download/{tag}")
    };
    let temp = tempfile::tempdir().context("create MyDB update staging directory")?;
    let archive_path = temp.path().join(asset.archive_name);
    let checksum_path = temp.path().join(format!("{}.sha256", asset.archive_name));
    download_file(
        &format!("{release_base}/{}", asset.archive_name),
        &archive_path,
    )?;
    download_file(
        &format!("{release_base}/{}.sha256", asset.archive_name),
        &checksum_path,
    )?;
    let checksum = verify_sha256(&archive_path, &checksum_path)?;
    let extract_dir = temp.path().join("package");
    fs::create_dir(&extract_dir).context("create MyDB update extraction directory")?;
    extract_archive(asset.kind, &archive_path, &extract_dir, temp.path())?;
    let source_dir = find_source_dir(&extract_dir, asset)?;

    let current_exe = env::current_exe().context("locate current MyDB CLI")?;
    let install_dir = options
        .install_dir
        .clone()
        .or_else(|| current_exe.parent().map(Path::to_path_buf))
        .ok_or_else(|| anyhow::anyhow!("current MyDB CLI has no parent directory"))?;
    if !install_dir.is_dir() {
        bail!(
            "MyDB install directory does not exist: {}",
            install_dir.display()
        );
    }

    if options.check {
        println!(
            "Update available: {} ({}, SHA-256 {})",
            if tag == "latest" { "latest" } else { &tag },
            asset.archive_name,
            checksum
        );
        return Ok(());
    }

    let temp_root = temp.keep();
    let service_name = options
        .service_name
        .clone()
        .unwrap_or_else(default_service_name);
    let child_result = if cfg!(windows) {
        spawn_windows_update(
            &source_dir,
            &archive_path,
            &install_dir,
            &service_name,
            &temp_root,
        )
    } else {
        spawn_unix_update(
            &source_dir,
            &archive_path,
            &install_dir,
            &service_name,
            &temp_root,
        )
    };
    if let Err(error) = child_result {
        let _ = fs::remove_dir_all(&temp_root);
        return Err(error);
    }
    Ok(())
}

fn release_asset() -> Result<ReleaseAsset> {
    let arch = match env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("MyDB update does not support architecture {other}"),
    };
    if cfg!(target_os = "windows") {
        return Ok(ReleaseAsset {
            archive_name: if arch == "x86_64" {
                "mydb-windows-x86_64.zip"
            } else {
                "mydb-windows-aarch64.zip"
            },
            installer_name: "install.ps1",
            kind: ArchiveKind::Zip,
        });
    }
    if cfg!(target_os = "linux") {
        return Ok(ReleaseAsset {
            archive_name: if arch == "x86_64" {
                "mydb-linux-x86_64.tar.gz"
            } else {
                "mydb-linux-aarch64.tar.gz"
            },
            installer_name: "install.sh",
            kind: ArchiveKind::TarGz,
        });
    }
    if cfg!(target_os = "macos") {
        return Ok(ReleaseAsset {
            archive_name: if arch == "x86_64" {
                "mydb-macos-x86_64.tar.gz"
            } else {
                "mydb-macos-aarch64.tar.gz"
            },
            installer_name: "install.sh",
            kind: ArchiveKind::TarGz,
        });
    }
    bail!(
        "MyDB update does not support operating system {}",
        env::consts::OS
    )
}

fn normalize_tag(version: &str) -> Result<String> {
    let version = version.trim();
    if version.eq_ignore_ascii_case("latest") {
        return Ok("latest".into());
    }
    if version.is_empty()
        || version.len() > 64
        || !version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
    {
        bail!("invalid release version '{version}'")
    }
    Ok(if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    })
}

fn download_file(url: &str, destination: &Path) -> Result<()> {
    let curl = if cfg!(windows) { "curl.exe" } else { "curl" };
    let curl_status = Command::new(curl)
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(destination)
        .arg(url)
        .status();
    if curl_status.is_ok_and(|status| status.success()) {
        return Ok(());
    }
    if cfg!(windows) {
        let powershell_status = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "$ProgressPreference='SilentlyContinue'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -Uri $env:MYDB_UPDATE_URL -OutFile $env:MYDB_UPDATE_PATH -UseBasicParsing",
            ])
            .env("MYDB_UPDATE_URL", url)
            .env("MYDB_UPDATE_PATH", destination)
            .status();
        if powershell_status.is_ok_and(|status| status.success()) {
            return Ok(());
        }
    }
    let wget_status = Command::new("wget")
        .args(["--quiet", "--output-document"])
        .arg(destination)
        .arg(url)
        .status();
    if wget_status.is_ok_and(|status| status.success()) {
        return Ok(());
    }
    bail!("download failed; install curl or wget and retry ({url})")
}

fn verify_sha256(archive_path: &Path, checksum_path: &Path) -> Result<String> {
    let expected = fs::read_to_string(checksum_path)?
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow::anyhow!("empty SHA-256 sidecar"))?;
    if expected.len() != 64
        || !expected
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        bail!("invalid SHA-256 sidecar for {}", archive_path.display());
    }
    let mut file = File::open(archive_path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual = format!("{:x}", digest.finalize());
    if actual != expected {
        bail!(
            "SHA-256 verification failed for {}: expected {}, got {}",
            archive_path.display(),
            expected,
            actual
        );
    }
    Ok(actual)
}

fn extract_archive(
    kind: ArchiveKind,
    archive_path: &Path,
    destination: &Path,
    temp_root: &Path,
) -> Result<()> {
    match kind {
        ArchiveKind::TarGz => {
            let listing = Command::new("tar")
                .args(["--list", "--gzip", "--file"])
                .arg(archive_path)
                .output()
                .context("list MyDB update archive")?;
            if !listing.status.success() {
                bail!("tar failed while listing {}", archive_path.display());
            }
            validate_archive_listing(&String::from_utf8_lossy(&listing.stdout))?;
            let status = Command::new("tar")
                .args(["--extract", "--gzip", "--file"])
                .arg(archive_path)
                .args(["--directory"])
                .arg(destination)
                .status()
                .context("run tar for MyDB update")?;
            if !status.success() {
                bail!("tar failed while extracting {}", archive_path.display());
            }
        }
        ArchiveKind::Zip => {
            let script_path = temp_root.join("extract-update.ps1");
            fs::write(
                &script_path,
                "param([string]$Archive,[string]$Destination)\n$ErrorActionPreference = 'Stop'\nAdd-Type -AssemblyName System.IO.Compression.FileSystem\n$zip = [System.IO.Compression.ZipFile]::OpenRead($Archive)\ntry { $zip.Entries | ForEach-Object { $_.FullName } } finally { $zip.Dispose() }\n",
            )?;
            let listing = Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&script_path)
                .arg(archive_path)
                .arg(destination)
                .output()
                .context("run PowerShell to extract MyDB update")?;
            if !listing.status.success() {
                bail!("PowerShell failed while listing {}", archive_path.display());
            }
            validate_archive_listing(&String::from_utf8_lossy(&listing.stdout))?;
            let extract_script = temp_root.join("extract-update.ps1");
            fs::write(
                &extract_script,
                "param([string]$Archive,[string]$Destination)\n$ErrorActionPreference = 'Stop'\nExpand-Archive -LiteralPath $Archive -DestinationPath $Destination -Force\n",
            )?;
            let status = Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&extract_script)
                .arg(archive_path)
                .arg(destination)
                .status()
                .context("extract MyDB update archive")?;
            if !status.success() {
                bail!("Expand-Archive failed for {}", archive_path.display());
            }
        }
    }
    Ok(())
}

fn validate_archive_listing(listing: &str) -> Result<()> {
    for raw_entry in listing.lines() {
        let entry = raw_entry.trim();
        let normalized = entry
            .strip_prefix("./")
            .or_else(|| entry.strip_prefix(".\\"))
            .unwrap_or(entry);
        if normalized.is_empty() {
            continue;
        }
        let starts_with_backslash = normalized.starts_with('\\');
        let is_tar_octal_escape = normalized.as_bytes().first() == Some(&b'\\')
            && normalized
                .as_bytes()
                .get(1..4)
                .is_some_and(|bytes| bytes.iter().all(u8::is_ascii_digit));
        if normalized.starts_with('/')
            || (starts_with_backslash && !is_tar_octal_escape)
            || normalized.contains(':')
            || normalized.split(['/', '\\']).any(|part| part == "..")
        {
            bail!("refusing archive path outside the update staging directory: {entry}");
        }
    }
    Ok(())
}

fn find_source_dir(extract_dir: &Path, asset: ReleaseAsset) -> Result<PathBuf> {
    let server_name = binary_name("mydb-server");
    if is_regular_file(&extract_dir.join(&server_name)) {
        return validate_source_dir(extract_dir, asset);
    }
    for entry in fs::read_dir(extract_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && is_regular_file(&entry.path().join(&server_name)) {
            return validate_source_dir(&entry.path(), asset);
        }
    }
    bail!(
        "release archive does not contain {} and cannot be installed",
        server_name
    )
}

fn validate_source_dir(source_dir: &Path, asset: ReleaseAsset) -> Result<PathBuf> {
    for stem in [
        "mydb-server",
        "mydb-cli",
        "mydb",
        "mydb-migrate",
        "mydbdump",
    ] {
        let path = source_dir.join(binary_name(stem));
        if !is_regular_file(&path) {
            bail!("release package is missing {}", path.display());
        }
    }
    let installer = source_dir.join(asset.installer_name);
    if !is_regular_file(&installer) {
        bail!("release package is missing {}", installer.display());
    }
    for legacy in ["mydb-router", "mydb-router.exe"] {
        if source_dir.join(legacy).exists() {
            bail!("refusing package containing legacy router binary");
        }
    }
    Ok(source_dir.to_path_buf())
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

fn binary_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn default_service_name() -> String {
    if cfg!(windows) {
        "MyDBServer".into()
    } else {
        "mydb".into()
    }
}

fn spawn_windows_update(
    source_dir: &Path,
    archive_path: &Path,
    install_dir: &Path,
    service_name: &str,
    temp_root: &Path,
) -> Result<()> {
    let script = source_dir.join("install.ps1");
    let log_path = install_dir.join("mydb-update.log");
    let log = File::create(&log_path)
        .with_context(|| format!("create update log {}", log_path.display()))?;
    Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(script)
        .args(["-BinariesOnly", "-PackagePath"])
        .arg(archive_path)
        .args(["-InstallDir"])
        .arg(install_dir)
        .args(["-ServiceName"])
        .arg(service_name)
        .args(["-WaitForProcessId"])
        .arg(std::process::id().to_string())
        .args(["-UpdateTempRoot"])
        .arg(temp_root)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("start Windows MyDB update helper")?;
    println!(
        "MyDB update scheduled; close this process and wait for the helper. Log: {}",
        log_path.display()
    );
    Ok(())
}

fn spawn_unix_update(
    source_dir: &Path,
    archive_path: &Path,
    install_dir: &Path,
    service_name: &str,
    temp_root: &Path,
) -> Result<()> {
    let script = source_dir.join("install.sh");
    let status = Command::new("bash")
        .arg(script)
        .arg("update")
        .env("PACKAGE_PATH", archive_path)
        .env("INSTALL_DIR", install_dir)
        .env("SERVICE_NAME", service_name)
        .status()
        .context("run MyDB Unix update helper")?;
    let cleanup_result = fs::remove_dir_all(temp_root);
    if !status.success() {
        let _ = cleanup_result;
        bail!("MyDB update helper failed with status {status}");
    }
    cleanup_result.context("remove MyDB update staging directory")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn normalizes_safe_release_tags() {
        assert_eq!(normalize_tag("latest").expect("latest tag"), "latest");
        assert_eq!(normalize_tag("0.1.5").expect("version tag"), "v0.1.5");
        assert_eq!(normalize_tag("v0.1.5").expect("version tag"), "v0.1.5");
        assert!(normalize_tag("v0.1.5/../../secret").is_err());
    }

    #[test]
    fn verifies_checksum_sidecar_without_using_filename() {
        let temp = tempfile::tempdir().expect("checksum temp directory");
        let archive = temp.path().join("package.tar.gz");
        let sidecar = temp.path().join("package.tar.gz.sha256");
        let mut file = File::create(&archive).expect("create archive fixture");
        file.write_all(b"mydb-update-fixture")
            .expect("write archive fixture");
        let mut digest = Sha256::new();
        digest.update(b"mydb-update-fixture");
        let expected = format!("{:x}", digest.finalize());
        fs::write(&sidecar, format!("{expected} another-name.tar.gz\n"))
            .expect("write checksum fixture");
        assert_eq!(
            verify_sha256(&archive, &sidecar).expect("verify checksum"),
            expected
        );
    }

    #[test]
    fn rejects_legacy_router_and_missing_installer() {
        let temp = tempfile::tempdir().expect("package temp directory");
        for stem in [
            "mydb-server",
            "mydb-cli",
            "mydb",
            "mydb-migrate",
            "mydbdump",
        ] {
            File::create(temp.path().join(binary_name(stem))).expect("create binary fixture");
        }
        let asset = ReleaseAsset {
            archive_name: "mydb-linux-x86_64.tar.gz",
            installer_name: "install.sh",
            kind: ArchiveKind::TarGz,
        };
        assert!(validate_source_dir(temp.path(), asset).is_err());
        fs::write(temp.path().join("install.sh"), "#!/bin/sh\n").expect("create installer");
        assert!(validate_source_dir(temp.path(), asset).is_ok());
        File::create(temp.path().join("mydb-router")).expect("create legacy fixture");
        assert!(validate_source_dir(temp.path(), asset).is_err());
    }

    #[test]
    fn rejects_archive_path_traversal() {
        assert!(validate_archive_listing("./\nmydb-server\n../outside\n").is_err());
        assert!(validate_archive_listing("/absolute/path\n").is_err());
        assert!(validate_archive_listing("mydb-server\nmydb-cli\n").is_ok());
    }

    #[test]
    fn accepts_archive_root_entry() {
        assert!(validate_archive_listing("./\n").is_ok());
        assert!(validate_archive_listing(".\\\n").is_ok());
        assert!(validate_archive_listing("./\\346\\200\\247\\350.md\n").is_ok());
    }
}
