# ClipIt

ClipIt 是一个用 Rust 编写的轻量局域网文件传输工具。发送入口位于 Windows
资源管理器或 macOS Finder 的文件右键菜单；设备选择界面使用仅绑定
`127.0.0.1` 的临时网页，因此不需要打包 Chromium、Electron 或大型 GUI 框架。

> 当前是可运行的 MVP，定位为可信私有局域网工具。文件内容不加密，以降低 CPU、
> 协议和程序体积开销；请勿在公共 Wi-Fi、访客网络或不可信局域网中使用。
> BLAKE3 在这里用于发现传输损坏，不提供身份认证或防窃听能力。

## 特性

- 单个 Rust 可执行文件，release 配置启用 LTO、strip 和体积优化。
- 文件和目录递归传输，1 MiB 流式缓冲，不把完整文件读入内存。
- UDP 组播自动发现设备；TCP 长连接持续发送所有内容。
- 文件先写入 `.clipit-part` 临时文件，通过 BLAKE3 校验后再原子改名。
- 拒绝绝对路径、`..`、Windows 盘符和反斜线等不安全远端路径。
- Windows 右键菜单写入当前用户注册表，无需管理员权限。
- macOS 使用当前用户的 Finder 快速操作，无需系统扩展。

## 与 Synergy 3 / LocalSend 共存

ClipIt 没有调用或修改它们的进程、配置、剪贴板、服务发现或端口：

| 功能 | ClipIt |
|---|---:|
| 设备发现 | UDP 组播 `239.255.42.89:42489` |
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

## 使用

接收端和发送端都建议常驻运行：

```powershell
clip-it serve
```

查看设备、直接发送：

```powershell
clip-it devices
clip-it send --device MY-MAC .\video.mp4 .\photos
# 或跳过发现
clip-it send --to 192.168.1.20:42490 .\video.mp4
```

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

## 下一里程碑

1. 可选设备允许列表与接收确认，避免误发，但不加密文件载荷。
2. Windows 登录启动 / macOS LaunchAgent 与托盘状态。
3. 断点续传、并发分块和 10/25 GbE 吞吐基准。
4. 签名安装包、自动更新，以及 Windows 11 原生顶层菜单扩展。
