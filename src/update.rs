use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MANIFEST_URL: &str =
    "https://github.com/Diomchen/clip_it/releases/latest/download/update-manifest.json";
const SIGNATURE_URL: &str =
    "https://github.com/Diomchen/clip_it/releases/latest/download/update-manifest.sig";
const USER_AGENT: &str = concat!("ClipIt/", env!("CARGO_PKG_VERSION"));
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_SIGNATURE_BYTES: usize = 4096;
const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;

// The matching private key lives only in the GitHub Actions
// UPDATE_SIGNING_PRIVATE_KEY secret and is never stored in the repository.
const UPDATE_PUBLIC_KEY: [u8; 32] = [
    0x1d, 0xbc, 0x1e, 0x1f, 0xa4, 0x07, 0x59, 0x8f, 0x29, 0xc8, 0xc5, 0xf4, 0xd5, 0x5a, 0xae, 0x13,
    0x42, 0x5d, 0x14, 0xc9, 0x49, 0x55, 0xac, 0xd9, 0x6c, 0x4f, 0x12, 0x52, 0xdc, 0xa6, 0xc9, 0xe8,
];

#[derive(Clone, Debug, Deserialize)]
struct UpdateManifest {
    schema: u32,
    version: String,
    artifacts: BTreeMap<String, UpdateArtifact>,
}

#[derive(Clone, Debug, Deserialize)]
struct UpdateArtifact {
    url: String,
    sha256: String,
    size: u64,
}

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    artifact: UpdateArtifact,
}

pub fn check_for_update() -> Result<Option<UpdateInfo>> {
    let manifest_bytes =
        download_small(MANIFEST_URL, MAX_MANIFEST_BYTES).context("下载更新清单失败")?;
    let signature =
        download_small(SIGNATURE_URL, MAX_SIGNATURE_BYTES).context("下载更新签名失败")?;
    verify_manifest(&manifest_bytes, &signature)?;

    let manifest: UpdateManifest =
        serde_json::from_slice(&manifest_bytes).context("更新清单格式无效")?;
    if manifest.schema != 1 {
        bail!("不支持的更新清单版本 {}", manifest.schema);
    }
    let remote = Version::parse(&manifest.version).context("更新版本号无效")?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).context("当前版本号无效")?;
    if remote <= current {
        return Ok(None);
    }
    let key = platform_artifact_key()?;
    let artifact = manifest
        .artifacts
        .get(key)
        .with_context(|| format!("更新清单缺少当前平台安装包 {key}"))?
        .clone();
    validate_artifact(&artifact)?;
    Ok(Some(UpdateInfo {
        version: remote.to_string(),
        artifact,
    }))
}

pub fn install_latest(config_dir: &Path, parent_pid: Option<u32>) -> Result<String> {
    let update = check_for_update()?.context("当前已是最新版本")?;
    let update_dir = config_dir.join("updates").join(&update.version);
    fs::create_dir_all(&update_dir).context("创建更新暂存目录失败")?;
    let filename = update
        .artifact
        .url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && !name.contains(['/', '\\']))
        .context("更新安装包名称无效")?;
    let destination = update_dir.join(filename);
    download_artifact(&update.artifact, &destination)?;
    stage_install(&destination, &update_dir, parent_pid)?;
    Ok(update.version)
}

fn download_small(url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("请求 {url} 失败"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("远端响应超过大小限制");
    }
    Ok(bytes)
}

fn verify_manifest(manifest: &[u8], signature_text: &[u8]) -> Result<()> {
    verify_manifest_with_key(manifest, signature_text, &UPDATE_PUBLIC_KEY)
}

fn verify_manifest_with_key(
    manifest: &[u8],
    signature_text: &[u8],
    public_key: &[u8; 32],
) -> Result<()> {
    let encoded = std::str::from_utf8(signature_text)
        .context("更新签名不是 UTF-8")?
        .trim();
    let signature_bytes = STANDARD.decode(encoded).context("更新签名编码无效")?;
    let signature = Signature::from_slice(&signature_bytes).context("更新签名长度无效")?;
    let verifying_key = VerifyingKey::from_bytes(public_key).context("更新公钥无效")?;
    verifying_key
        .verify(manifest, &signature)
        .context("更新清单签名验证失败")
}

fn validate_artifact(artifact: &UpdateArtifact) -> Result<()> {
    if !artifact
        .url
        .starts_with("https://github.com/Diomchen/clip_it/releases/download/")
    {
        bail!("更新下载地址不受信任");
    }
    if artifact.size == 0 || artifact.size > MAX_UPDATE_BYTES {
        bail!("更新安装包大小无效");
    }
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("更新安装包 SHA-256 无效");
    }
    Ok(())
}

fn download_artifact(artifact: &UpdateArtifact, destination: &Path) -> Result<()> {
    validate_artifact(artifact)?;
    let temporary = destination.with_extension("download");
    let response = ureq::get(&artifact.url)
        .set("User-Agent", USER_AGENT)
        .call()
        .context("下载更新安装包失败")?;
    let mut reader = response.into_reader().take(artifact.size.saturating_add(1));
    let mut file = fs::File::create(&temporary).context("创建更新临时文件失败")?;
    let copied = std::io::copy(&mut reader, &mut file)?;
    file.flush()?;
    if copied != artifact.size {
        let _ = fs::remove_file(&temporary);
        bail!("更新安装包大小与签名清单不一致");
    }
    let digest = sha256_file(&temporary)?;
    if !digest.eq_ignore_ascii_case(&artifact.sha256) {
        let _ = fs::remove_file(&temporary);
        bail!("更新安装包 SHA-256 校验失败");
    }
    if destination.exists() {
        fs::remove_file(destination).context("移除旧更新暂存文件失败")?;
    }
    fs::rename(&temporary, destination).context("保存更新安装包失败")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(target_os = "windows")]
fn platform_artifact_key() -> Result<&'static str> {
    if cfg!(target_arch = "x86_64") {
        Ok("windows-x86_64-exe")
    } else {
        bail!("当前 Windows 架构尚不支持自动更新")
    }
}

#[cfg(target_os = "macos")]
fn platform_artifact_key() -> Result<&'static str> {
    Ok("macos-universal-zip")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_artifact_key() -> Result<&'static str> {
    bail!("当前平台尚不支持自动更新")
}

#[cfg(target_os = "windows")]
fn stage_install(artifact: &Path, update_dir: &Path, parent_pid: Option<u32>) -> Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let executable = std::env::current_exe().context("无法确定当前程序路径")?;
    let script = update_dir.join("install-update.ps1");
    fs::write(
        &script,
        r#"param([int]$ParentPid,[int]$UpdaterPid,[string]$Source,[string]$Destination)
$ErrorActionPreference = 'Stop'
if ($ParentPid -gt 0) { Stop-Process -Id $ParentPid -Force -ErrorAction SilentlyContinue }
Get-Process -ErrorAction SilentlyContinue | ForEach-Object {
  try {
    if ($_.Id -ne $UpdaterPid -and $_.Path -ieq $Destination) {
      Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
    }
  } catch {}
}
Wait-Process -Id $UpdaterPid -ErrorAction SilentlyContinue
for ($attempt = 0; $attempt -lt 30; $attempt++) {
  try { Copy-Item -LiteralPath $Source -Destination $Destination -Force; break }
  catch { if ($attempt -eq 29) { throw }; Start-Sleep -Milliseconds 300 }
}
Start-Process -FilePath $Destination
Remove-Item -LiteralPath $Source -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
"#,
    )?;
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&script)
        .arg("-ParentPid")
        .arg(parent_pid.unwrap_or(0).to_string())
        .arg("-UpdaterPid")
        .arg(std::process::id().to_string())
        .arg("-Source")
        .arg(artifact)
        .arg("-Destination")
        .arg(executable)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("启动 Windows 更新安装器失败")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn stage_install(artifact: &Path, update_dir: &Path, parent_pid: Option<u32>) -> Result<()> {
    let executable = std::env::current_exe().context("无法确定当前程序路径")?;
    let bundle = executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .context("当前程序不在 ClipIt.app 中，无法自动替换")?
        .to_path_buf();
    let extracted = update_dir.join("extracted");
    if extracted.exists() {
        fs::remove_dir_all(&extracted).context("清理旧更新解压目录失败")?;
    }
    fs::create_dir_all(&extracted)?;
    let status = Command::new("ditto")
        .args(["-x", "-k"])
        .arg(artifact)
        .arg(&extracted)
        .status()
        .context("解压 macOS 更新失败")?;
    if !status.success() {
        bail!("解压 macOS 更新失败");
    }
    let source = extracted.join("ClipIt.app");
    if !source.join("Contents/MacOS/clip-it").is_file() {
        bail!("macOS 更新包缺少 ClipIt.app");
    }
    let script = update_dir.join("install-update.sh");
    fs::write(
        &script,
        r#"#!/bin/zsh
set -eu
parent_pid="$1"
updater_pid="$2"
source_app="$3"
destination_app="$4"
backup_app="${destination_app}.clipit-backup"
if [[ "$parent_pid" != "0" ]]; then kill "$parent_pid" 2>/dev/null || true; fi
while kill -0 "$updater_pid" 2>/dev/null; do sleep 0.2; done
rm -rf "$backup_app"
mv "$destination_app" "$backup_app"
if ditto "$source_app" "$destination_app"; then
  rm -rf "$backup_app"
  open "$destination_app"
else
  rm -rf "$destination_app"
  mv "$backup_app" "$destination_app"
  exit 1
fi
rm -rf "$(dirname "$source_app")"
rm -f "$0"
"#,
    )?;
    let status = Command::new("chmod").arg("700").arg(&script).status()?;
    if !status.success() {
        bail!("设置 macOS 更新脚本权限失败");
    }
    Command::new(&script)
        .arg(parent_pid.unwrap_or(0).to_string())
        .arg(std::process::id().to_string())
        .arg(source)
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("启动 macOS 更新安装器失败")?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn stage_install(_artifact: &Path, _update_dir: &Path, _parent_pid: Option<u32>) -> Result<()> {
    bail!("当前平台尚不支持自动安装更新")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn accepts_valid_manifest_signature_and_rejects_tampering() {
        let signing = SigningKey::from_bytes(&[7_u8; 32]);
        let manifest = br#"{"schema":1,"version":"9.9.9","artifacts":{}}"#;
        let signature = STANDARD.encode(signing.sign(manifest).to_bytes());
        let public = signing.verifying_key().to_bytes();

        assert!(verify_manifest_with_key(manifest, signature.as_bytes(), &public).is_ok());
        assert!(verify_manifest_with_key(b"tampered", signature.as_bytes(), &public).is_err());
    }

    #[test]
    fn rejects_untrusted_or_malformed_artifacts() {
        let mut artifact = UpdateArtifact {
            url: "https://example.com/update.exe".into(),
            sha256: "0".repeat(64),
            size: 1,
        };
        assert!(validate_artifact(&artifact).is_err());
        artifact.url = "https://github.com/Diomchen/clip_it/releases/download/v1/update.exe".into();
        assert!(validate_artifact(&artifact).is_ok());
        artifact.sha256 = "not-a-hash".into();
        assert!(validate_artifact(&artifact).is_err());
    }
}
