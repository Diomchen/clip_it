# ClipIt

ClipIt 是一个用 Rust 编写的轻量局域网文件传输工具。发送入口位于 Windows
资源管理器或 macOS Finder 的文件右键菜单；设备选择界面使用仅绑定
`127.0.0.1` 的临时网页，因此不需要打包 Chromium、Electron 或大型 GUI 框架。

> 当前是可运行的 MVP，定位为可信私有局域网工具。文件内容不加密，以降低 CPU、
> 协议和程序体积开销；请勿在公共 Wi-Fi、访客网络或不可信局域网中使用。
> BLAKE3 在这里用于发现传输损坏，不提供身份认证或防窃听能力。

## 特性

- 单个 Rust 可执行文件，release 配置启用 LTO、strip 和体积优化。
- 文件和目录递归传输，32 MiB 分块、4 条并发流，不把完整文件读入内存。
- UDP 组播与局域网广播自动发现设备；TCP 并行发送缺失分块。
- 接收端持久化 `.clipit-resume.json`，重试时只补缺失分块；每块通过 BLAKE3
  校验后再完成原子改名。
- 拒绝绝对路径、`..`、Windows 盘符和反斜线等不安全远端路径。
- Windows 右键菜单写入当前用户注册表，无需管理员权限。
- macOS 使用当前用户的 Finder 快速操作，无需系统扩展。
- 未信任设备发送前在仅绑定本机的网页中确认，可选择仅本次或永久允许。
- 支持可信设备白名单，以及确认、仅可信设备、全部接受三种接收策略。
- Windows 系统托盘 / macOS 菜单栏显示运行状态，可重启服务、修改设置和开机启动。
- 自动监听系统剪贴板，将文本及复制的任意普通文件/目录同步到所有在线 ClipIt 设备。
- Apple 风格跨平台应用图标；设置页以可拖拽水泡实时呈现局域网设备与连接状态。
- 每台设备可自定义 Emoji 节点图标和显示名称，并通过局域网发现同步展示。

## 与 Synergy 3 / LocalSend 共存

ClipIt 没有调用或修改它们的进程、配置、剪贴板、服务发现或端口：

| 功能 | ClipIt |
|---|---:|
| 设备发现 | UDP 组播 `239.255.42.89:42489` + 局域网广播 |
| 文件传输 | TCP `0.0.0.0:42490` |
| 本地选择页 | 随机 `127.0.0.1` 临时端口 |
| 配置目录 | 系统配置目录下的 `clip-it/` |

LocalSend 通常使用 `53317`，Synergy 通常使用 `24800`；ClipIt 不占用这些端口。

## 构建

```powershell
cargo build --release
```

生成的程序位于 `target/release/clip-it.exe`（Windows）或
`target/release/clip-it`（macOS）。正式安装右键菜单前，请先把它复制到不会移动
的用户程序目录，因为菜单会记录当前可执行文件的绝对路径。

### 安装包

macOS 在本机生成 DMG（默认生成当前 CPU 架构）：

```bash
./scripts/package-macos.sh
```

生成的 `dist/ClipIt-<版本>-macos-<架构>.dmg` 内含 `ClipIt.app` 和应用程序目录
快捷方式。把应用拖到 Applications 后双击，程序会自动安装 Finder 右键快速操作、
启用登录启动并进入菜单栏，无需执行终端命令。

Windows 可在 PowerShell 中运行 `./scripts/package-windows.ps1`，生成独立 EXE、ZIP
和 SHA-256 校验文件。所有安装包都在 `dist/` 下；未配置代码签名的构建为未签名
版本，macOS 首次启动时需要右键应用并选择“打开”。

## GitHub CI/CD

`.github/workflows/release.yml` 会在 PR 上执行格式、测试和 Clippy 检查；推送到
`main` 或手动触发时构建 Windows x86_64 EXE/ZIP 和 macOS arm64+x86_64 通用
DMG，并保存为 Actions artifacts。推送与 `Cargo.toml` 版本对应的标签会自动创建
GitHub Release：

```bash
git tag v0.4.2
git push origin v0.4.2
```

无 Apple 证书也能构建 ad-hoc 签名的 DMG。若需要公开分发时的 Developer ID
签名和 Apple 公证，请在仓库 Actions secrets 中配置：

- `MACOS_CERTIFICATE_BASE64`：Developer ID Application `.p12` 的 Base64 内容
- `MACOS_CERTIFICATE_PASSWORD`：`.p12` 密码
- `MACOS_SIGN_IDENTITY`：完整签名身份，例如 `Developer ID Application: ...`
- `APPLE_ID`、`APPLE_APP_PASSWORD`、`APPLE_TEAM_ID`：Apple 公证凭据

## 使用

直接启动 `clip-it` 或双击 `ClipIt.app` 会自动安装文件右键菜单、启用登录启动，
进入托盘/菜单栏模式并启动后台接收服务。菜单中可以查看状态、打开接收目录、
重启服务、修改设置和配置登录启动。

需要在终端以前台方式运行时使用：

```powershell
clip-it serve
```

无人值守设备可只允许可信列表，或显式恢复旧版的全部接收行为：

```powershell
clip-it serve --receive-policy trusted-only
clip-it serve --receive-policy accept-all
```

设置页仅绑定 `127.0.0.1`，可从托盘打开，也可以运行：

```powershell
clip-it configure
clip-it startup install
clip-it startup remove
clip-it startup status
```

设置页支持修改节点 Emoji、显示名称、文件传输端口、接收策略和剪贴板自动同步
开关，并实时展示局域网设备。在线设备以独立水泡出现；拖入本机水泡会建立双向
连接并播放融合动画，拖出时会同步断开双方连接。连接关系在双方设备中保存 7 天，
期间设备离线后再次上线会自动连接；到期后需重新拖动连接。端口更新后托盘会重启
后台服务。

### 自动剪贴板同步

在任一设备使用 `Ctrl+C`、`Command+C` 或鼠标菜单复制后：

- 文本自动写入所有已连接且在线的 ClipIt 设备的系统剪贴板，可直接粘贴。
- PixPin 等截图工具写入的内存位图会编码为 PNG 同步，并在接收端恢复为图片剪贴板，
  无需先保存为文件。
- 文件和目录自动通过现有流式传输协议发送；接收完成后，对方剪贴板会指向新文件，
  可直接粘贴到 Finder 或资源管理器。
- 文件扩展名和内容类型不受限制，目录递归发送；符号链接和特殊设备文件仍会跳过。
- 远端剪贴板写入带有短时去重，避免设备之间反复回传。

剪贴板同步按当前产品要求面向可信内网，会发送给所有已连接且在线的 ClipIt 设备；
连接关系与用于接收确认的可信设备列表相互独立。文本单次上限为 1 MiB，文件使用
流式传输，不把完整文件加载进内存。

查看设备、直接发送：

```powershell
clip-it devices
clip-it send --device MY-MAC .\video.mp4 .\photos
# 或跳过发现
clip-it send --to 192.168.1.20:42490 .\video.mp4
```

相同文件中断后重新执行同一条发送命令，ClipIt 会使用稳定传输 ID 读取接收端断点
状态，并仅发送尚未完成的 32 MiB 分块。文件大小或修改时间变化时会自动建立新的
传输会话。

### 10/25 GbE 吞吐基准

基准命令只测试内存到内存的 TCP 吞吐，不写磁盘。10 GbE 建议使用 4 GiB 和 4 条流，
25 GbE 建议使用 16 GiB 和 8 条流：

```powershell
clip-it benchmark --device MY-PC --size-gib 4 --streams 4
clip-it benchmark --to 192.168.1.20:42490 --size-gib 16 --streams 8
```

完整测试矩阵、系统调优建议和结果记录方式见
[`docs/benchmarks.md`](docs/benchmarks.md)。

`devices` 输出包含设备 UUID。可以预先管理接收端的可信设备列表：

```powershell
clip-it trust list
clip-it trust add 550e8400-e29b-41d4-a716-446655440000 --name MY-PC
clip-it trust remove 550e8400-e29b-41d4-a716-446655440000
clip-it trust clear
```

也可以在接收确认页点击“始终允许此设备”。可信列表保存在配置目录的
`trusted-devices.json`。设备 UUID 和名称由发送端自行声明，未做密码学认证；该机制
用于避免可信局域网中的误发，不能防御主动伪装设备。

安装当前用户的右键菜单 / Finder 快速操作：

```powershell
clip-it integrate install
clip-it integrate remove
```

右键选择“使用 ClipIt 发送”后，默认浏览器会打开一个仅本机可访问的设备选择页。
接收文件保存在用户下载目录的 `ClipIt/Incoming-<随机ID>/` 中。

测试或便携运行时可通过 `CLIP_IT_CONFIG_DIR` 和 `CLIP_IT_DOWNLOAD_DIR` 覆盖配置、
接收目录；默认用户不需要设置它们。

如果组播被路由器或系统防火墙拦截，仍可通过 `--to IP:42490` 直接传输。首次运行
时需要允许系统防火墙放行专用网络上的 UDP `42489` 和 TCP `42490`。

## 验证

```powershell
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## 已完成里程碑

- 可选设备允许列表与接收确认；默认确认未知设备，支持仅可信设备和全部接受策略。
- Windows 托盘 / macOS 菜单栏、服务重启、端口配置与登录启动。
- 文本及所有普通文件/目录的系统剪贴板自动同步。
- Apple 风格应用图标与可拖拽、融合/分离动画的局域网设备视图。
- 断点续传、32 MiB 分块、4 流并发传输和 10/25 GbE 基准工具。

## 下一里程碑

1. 签名安装包与自动更新。
2. Windows 11 原生顶层菜单扩展。
