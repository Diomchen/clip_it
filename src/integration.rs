#[cfg(target_os = "windows")]
use std::env;
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "macos")]
use std::{fs, path::PathBuf};

#[cfg(any(target_os = "windows", target_os = "macos"))]
use anyhow::Context;
use anyhow::Result;
#[cfg(not(target_os = "macos"))]
use anyhow::bail;

#[cfg(target_os = "windows")]
pub fn install() -> Result<()> {
    let executable = env::current_exe().context("无法确定 ClipIt 可执行文件路径")?;
    let clsid_key = format!(
        r"HKCU\Software\Classes\CLSID\{}",
        crate::windows_shell::SHELL_COMMAND_CLSID_TEXT
    );
    let server_command = format!("\"{}\" shell-server", executable.display());
    reg(&[
        "add",
        &clsid_key,
        "/ve",
        "/d",
        "ClipIt Explorer Command",
        "/f",
    ])?;
    reg(&[
        "add",
        &format!(r"{clsid_key}\LocalServer32"),
        "/ve",
        "/d",
        &server_command,
        "/f",
    ])?;

    for class in ["*", "Directory"] {
        let key = format!(r"HKCU\Software\Classes\{class}\shell\ClipIt");
        // Remove the pre-v0.5 legacy command before registering the Windows 11
        // IExplorerCommand handler. Ignore a missing key to keep this idempotent.
        let _ = Command::new("reg")
            .args(["delete", &format!(r"{key}\command"), "/f"])
            .status();
        reg(&["add", &key, "/v", "MUIVerb", "/d", "使用 ClipIt 发送", "/f"])?;
        reg(&[
            "add",
            &key,
            "/v",
            "Icon",
            "/d",
            executable.to_string_lossy().as_ref(),
            "/f",
        ])?;
        reg(&[
            "add",
            &key,
            "/v",
            "ExplorerCommandHandler",
            "/d",
            crate::windows_shell::SHELL_COMMAND_CLSID_TEXT,
            "/f",
        ])?;
        reg(&["add", &key, "/v", "MultiSelectModel", "/d", "Player", "/f"])?;
    }
    refresh_windows_shell();
    println!("已安装 Windows 11 资源管理器顶层右键菜单。无需管理员权限。");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn remove() -> Result<()> {
    for class in ["*", "Directory"] {
        let key = format!(r"HKCU\Software\Classes\{class}\shell\ClipIt");
        let status = Command::new("reg").args(["delete", &key, "/f"]).status()?;
        if !status.success() {
            eprintln!("右键菜单项不存在或删除失败: {key}");
        }
    }
    let clsid_key = format!(
        r"HKCU\Software\Classes\CLSID\{}",
        crate::windows_shell::SHELL_COMMAND_CLSID_TEXT
    );
    let status = Command::new("reg")
        .args(["delete", &clsid_key, "/f"])
        .status()?;
    if !status.success() {
        eprintln!("ClipIt COM 注册项不存在或删除失败: {clsid_key}");
    }
    refresh_windows_shell();
    println!("已移除 Windows 资源管理器右键菜单。");
    Ok(())
}

#[cfg(target_os = "windows")]
fn refresh_windows_shell() {
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

#[cfg(target_os = "windows")]
pub fn startup_install() -> Result<()> {
    let executable = env::current_exe().context("无法确定 ClipIt 可执行文件路径")?;
    let command = format!("\"{}\"", executable.display());
    reg(&[
        "add",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
        "/v",
        "ClipIt",
        "/d",
        &command,
        "/f",
    ])?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn startup_remove() -> Result<()> {
    if !startup_enabled() {
        return Ok(());
    }
    let status = Command::new("reg")
        .args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "ClipIt",
            "/f",
        ])
        .status()?;
    if !status.success() {
        bail!("移除 Windows 登录启动项失败");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn startup_enabled() -> bool {
    Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "ClipIt",
        ])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "windows")]
fn reg(args: &[&str]) -> Result<()> {
    let status = Command::new("reg").args(args).status()?;
    if !status.success() {
        bail!("写入 Windows 右键菜单失败");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn install() -> Result<()> {
    let executable = std::env::current_exe().context("无法确定 ClipIt 可执行文件路径")?;
    let workflow_dir = services_dir()?.join("Send with ClipIt.workflow");
    let contents_dir = workflow_dir.join("Contents");
    fs::create_dir_all(&contents_dir).context("创建 Finder 快速操作目录失败")?;

    let command = format!(
        "{} pick \"$@\" >/tmp/clip-it-picker.log 2>&1 &",
        shell_quote(executable.to_string_lossy().as_ref())
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>actions</key><array><dict><key>action</key><dict>
<key>AMAccepts</key><dict><key>Container</key><string>List</string><key>Optional</key><true/><key>Types</key><array><string>com.apple.cocoa.path</string></array></dict>
<key>AMActionVersion</key><string>2.0.3</string>
<key>BundleIdentifier</key><string>com.apple.RunShellScript</string>
<key>Class Name</key><string>RunShellScriptAction</string>
<key>UUID</key><string>0D8A307E-B0B4-4D31-A45C-4C4C49544954</string>
<key>parameters</key><dict>
<key>COMMAND_STRING</key><string>{}</string>
<key>CheckedForUserDefaultShell</key><true/>
<key>inputMethod</key><integer>1</integer>
<key>shell</key><string>/bin/zsh</string>
</dict></dict></dict></array>
<key>connectors</key><dict/>
<key>workflowMetaData</key><dict>
<key>serviceInputTypeIdentifier</key><string>com.apple.LSItemContentTypes</string>
<key>serviceOutputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
<key>serviceProcessesInput</key><integer>0</integer>
</dict></dict></plist>"#,
        xml_escape(&command)
    );
    let info = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.clip-it.SendWithClipIt</string>
<key>CFBundleName</key><string>Send with ClipIt</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>NSServices</key><array><dict>
<key>NSMenuItem</key><dict><key>default</key><string>使用 ClipIt 发送</string></dict>
<key>NSMessage</key><string>runWorkflowAsService</string>
<key>NSRequiredContext</key><dict><key>NSApplicationIdentifier</key><string>com.apple.finder</string></dict>
<key>NSSendFileTypes</key><array><string>public.item</string></array>
</dict></array>
</dict></plist>"#;
    fs::write(contents_dir.join("document.wflow"), document)?;
    fs::write(contents_dir.join("Info.plist"), info)?;
    println!("已安装 macOS Finder 快速操作；如未立即出现，请重新打开 Finder。");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn remove() -> Result<()> {
    let workflow_dir = services_dir()?.join("Send with ClipIt.workflow");
    if workflow_dir.exists() {
        fs::remove_dir_all(&workflow_dir).context("移除 Finder 快速操作失败")?;
    }
    println!("已移除 macOS Finder 快速操作。");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn startup_install() -> Result<()> {
    let executable = std::env::current_exe().context("无法确定 ClipIt 可执行文件路径")?;
    let path = launch_agent_path()?;
    let parent = path.parent().context("LaunchAgent 路径无效")?;
    fs::create_dir_all(parent).context("创建 LaunchAgents 目录失败")?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>dev.clip-it.agent</string>
<key>ProgramArguments</key><array><string>{}</string></array>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>"#,
        xml_escape(executable.to_string_lossy().as_ref())
    );
    fs::write(path, plist).context("写入 macOS 登录启动项失败")?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn startup_remove() -> Result<()> {
    let path = launch_agent_path()?;
    if path.exists() {
        fs::remove_file(path).context("移除 macOS 登录启动项失败")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn startup_enabled() -> bool {
    launch_agent_path().is_ok_and(|path| path.exists())
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("无法确定用户主目录")?
        .join("Library/LaunchAgents/dev.clip-it.agent.plist"))
}

#[cfg(target_os = "macos")]
fn services_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("无法确定用户主目录")?
        .join("Library")
        .join("Services"))
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn install() -> Result<()> {
    bail!("当前仅支持 Windows/macOS 文件管理器集成")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn remove() -> Result<()> {
    bail!("当前仅支持 Windows/macOS 文件管理器集成")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn startup_install() -> Result<()> {
    bail!("当前仅支持 Windows/macOS 登录启动")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn startup_remove() -> Result<()> {
    bail!("当前仅支持 Windows/macOS 登录启动")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn startup_enabled() -> bool {
    false
}
