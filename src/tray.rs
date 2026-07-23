use anyhow::Result;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use anyhow::bail;

use crate::config::AppConfig;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop {
    use std::{
        fs::OpenOptions,
        net::{Ipv4Addr, TcpListener},
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result};
    use tao::{
        event::{Event, StartCause},
        event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    };
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder,
        menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };

    use crate::{
        config::AppConfig,
        integration,
        protocol::TRAY_INSTANCE_PORT,
        update::{self, UpdateInfo},
    };

    #[derive(Debug)]
    enum UserEvent {
        Menu(MenuEvent),
        SettingsFinished,
        UpdateChecked(Result<Option<UpdateInfo>, String>),
        UpdateInstalled(Result<String, String>),
    }

    pub(super) fn run(config: AppConfig) -> Result<()> {
        let instance_guard = TcpListener::bind((Ipv4Addr::LOCALHOST, TRAY_INSTANCE_PORT))
            .context("ClipIt 托盘似乎已经在运行")?;
        instance_guard.set_nonblocking(true)?;

        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }));
        let settings_proxy = event_loop.create_proxy();
        let update_proxy = event_loop.create_proxy();

        let menu = Menu::new();
        let status = MenuItem::new(status_text(&config, true), false, None);
        let open_downloads = MenuItem::new("打开接收目录", true, None);
        let restart = MenuItem::new("重启后台服务", true, None);
        let configure = MenuItem::new("设置…", true, None);
        let update_item = MenuItem::new("正在检查更新…", false, None);
        let startup =
            CheckMenuItem::new("登录时自动启动", true, integration::startup_enabled(), None);
        let quit = MenuItem::new("退出 ClipIt", true, None);
        menu.append_items(&[
            &status,
            &PredefinedMenuItem::separator(),
            &open_downloads,
            &restart,
            &configure,
            &update_item,
            &startup,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;

        let mut child = spawn_service(&config)?;
        let mut current_config = config;
        let mut tray_icon: Option<TrayIcon> = None;
        let mut available_update: Option<UpdateInfo> = None;
        let mut update_busy = true;
        let mut next_health_check = Instant::now() + Duration::from_secs(1);
        let instance_guard = Some(instance_guard);

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::WaitUntil(next_health_check);
            let _keep_instance_guard = &instance_guard;

            match event {
                Event::NewEvents(StartCause::Init) => {
                    check_for_update(update_proxy.clone());
                    match TrayIconBuilder::new()
                        .with_menu(Box::new(menu.clone()))
                        .with_tooltip("ClipIt - 局域网剪贴板与文件同步")
                        .with_icon_as_template(cfg!(target_os = "macos"))
                        .with_icon(tray_icon_image())
                        .build()
                    {
                        Ok(icon) => tray_icon = Some(icon),
                        Err(error) => {
                            eprintln!("创建 ClipIt 托盘图标失败: {error}");
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                }
                Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                    next_health_check = Instant::now() + Duration::from_secs(1);
                    if child.try_wait().ok().flatten().is_some() {
                        status.set_text(status_text(&current_config, false));
                    }
                }
                Event::UserEvent(UserEvent::Menu(event)) => {
                    if event.id == *restart.id() {
                        restart_child(&mut child, &current_config, &status);
                    } else if event.id == *configure.id() {
                        spawn_settings(&current_config, settings_proxy.clone());
                    } else if event.id == *update_item.id() && !update_busy {
                        update_busy = true;
                        update_item.set_enabled(false);
                        if let Some(update) = &available_update {
                            update_item.set_text(format!("正在安装 v{}…", update.version));
                            install_update(current_config.config_dir.clone(), update_proxy.clone());
                        } else {
                            update_item.set_text("正在检查更新…");
                            check_for_update(update_proxy.clone());
                        }
                    } else if event.id == *open_downloads.id() {
                        if let Err(error) = open_path(&current_config.download_dir) {
                            eprintln!("打开接收目录失败: {error:#}");
                        }
                    } else if event.id == *startup.id() {
                        let enable = !integration::startup_enabled();
                        let result = if enable {
                            integration::startup_install()
                        } else {
                            integration::startup_remove()
                        };
                        match result {
                            Ok(()) => startup.set_checked(enable),
                            Err(error) => eprintln!("更新登录启动设置失败: {error:#}"),
                        }
                    } else if event.id == *quit.id() {
                        stop_child(&mut child);
                        tray_icon.take();
                        *control_flow = ControlFlow::Exit;
                    }
                }
                Event::UserEvent(UserEvent::SettingsFinished) => {
                    match AppConfig::load_or_create() {
                        Ok(config) => {
                            current_config = config;
                            restart_child(&mut child, &current_config, &status);
                        }
                        Err(error) => eprintln!("重新加载 ClipIt 设置失败: {error:#}"),
                    }
                }
                Event::UserEvent(UserEvent::UpdateChecked(result)) => {
                    update_busy = false;
                    update_item.set_enabled(true);
                    match result {
                        Ok(Some(info)) => {
                            update_item.set_text(format!("安装更新 v{}", info.version));
                            available_update = Some(info);
                        }
                        Ok(None) => {
                            update_item.set_text(format!(
                                "已是最新版本 v{}（点此检查）",
                                env!("CARGO_PKG_VERSION")
                            ));
                            available_update = None;
                        }
                        Err(error) => {
                            eprintln!("检查 ClipIt 更新失败: {error}");
                            update_item.set_text("检查更新失败（点此重试）");
                            available_update = None;
                        }
                    }
                }
                Event::UserEvent(UserEvent::UpdateInstalled(result)) => match result {
                    Ok(version) => {
                        update_item.set_text(format!("正在重启到 v{version}…"));
                        stop_child(&mut child);
                        tray_icon.take();
                        *control_flow = ControlFlow::Exit;
                    }
                    Err(error) => {
                        eprintln!("安装 ClipIt 更新失败: {error}");
                        update_busy = false;
                        update_item.set_text("安装更新失败（点此重试）");
                        update_item.set_enabled(true);
                    }
                },
                Event::LoopDestroyed => stop_child(&mut child),
                _ => {}
            }
        });
    }

    fn check_for_update(proxy: EventLoopProxy<UserEvent>) {
        std::thread::spawn(move || {
            let result = update::check_for_update().map_err(|error| format!("{error:#}"));
            let _ = proxy.send_event(UserEvent::UpdateChecked(result));
        });
    }

    fn install_update(config_dir: std::path::PathBuf, proxy: EventLoopProxy<UserEvent>) {
        std::thread::spawn(move || {
            let result =
                update::install_latest(&config_dir, None).map_err(|error| format!("{error:#}"));
            let _ = proxy.send_event(UserEvent::UpdateInstalled(result));
        });
    }

    fn spawn_service(config: &AppConfig) -> Result<Child> {
        std::fs::create_dir_all(&config.config_dir)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(config.config_dir.join("service.log"))?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("serve")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        suppress_windows_console(&mut command);
        command.spawn().context("启动 ClipIt 后台服务失败")
    }

    fn restart_child(child: &mut Child, config: &AppConfig, status: &MenuItem) {
        stop_child(child);
        match spawn_service(config) {
            Ok(new_child) => {
                *child = new_child;
                status.set_text(status_text(config, true));
            }
            Err(error) => {
                status.set_text(status_text(config, false));
                eprintln!("重启 ClipIt 后台服务失败: {error:#}");
            }
        }
    }

    fn stop_child(child: &mut Child) {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn spawn_settings(config: &AppConfig, proxy: EventLoopProxy<UserEvent>) {
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("无法打开设置: {error}");
                return;
            }
        };
        let config_dir = config.config_dir.clone();
        std::thread::spawn(move || {
            let mut command = Command::new(executable);
            command
                .arg("configure")
                .env("CLIP_IT_CONFIG_DIR", config_dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            suppress_windows_console(&mut command);
            let status = command.status();
            if status.is_ok_and(|status| status.success()) {
                let _ = proxy.send_event(UserEvent::SettingsFinished);
            }
        });
    }

    fn suppress_windows_console(command: &mut Command) {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;

            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(not(target_os = "windows"))]
        let _ = command;
    }

    fn status_text(config: &AppConfig, running: bool) -> String {
        let status = if running { "运行中" } else { "已停止" };
        let clipboard = if config.settings.clipboard_sync {
            "剪贴板同步已开启"
        } else {
            "剪贴板同步已关闭"
        };
        format!(
            "● {status} · 端口 {} · {clipboard}",
            config.settings.transfer_port
        )
    }

    fn tray_icon_image() -> Icon {
        let size = 32_u32;
        let mut rgba = vec![0_u8; (size * size * 4) as usize];
        for y in 4..28 {
            for x in 4..28 {
                let is_c = (x < 9 || !(9..23).contains(&y)) && !(x >= 20 && (9..23).contains(&y));
                let is_arrow = (x >= 15 && (13..=18).contains(&y))
                    || (x >= 21 && (9..=22).contains(&y) && x + y >= 32 && x >= y);
                if is_c || is_arrow {
                    let offset = ((y * size + x) * 4) as usize;
                    rgba[offset] = 37;
                    rgba[offset + 1] = 99;
                    rgba[offset + 2] = 235;
                    rgba[offset + 3] = 255;
                }
            }
        }
        Icon::from_rgba(rgba, size, size).expect("托盘图标像素尺寸固定有效")
    }

    #[cfg(target_os = "macos")]
    fn open_path(path: &std::path::Path) -> Result<()> {
        Command::new("open").arg(path).spawn()?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn open_path(path: &std::path::Path) -> Result<()> {
        Command::new("explorer").arg(path).spawn()?;
        Ok(())
    }
}

pub fn run(config: AppConfig) -> Result<()> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    return desktop::run(config);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = config;
        bail!("当前平台不支持托盘模式")
    }
}
