#![cfg(target_os = "windows")]

use std::{
    ffi::c_void,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{BOOL, CLASS_E_NOAGGREGATION, E_FAIL, E_NOTIMPL},
        System::Com::{
            CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoInitializeEx, CoRegisterClassObject,
            CoRevokeClassObject, CoTaskMemAlloc, CoTaskMemFree, CoUninitialize, IBindCtx,
            IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
        },
        UI::Shell::{
            ECF_DEFAULT, ECS_DISABLED, ECS_ENABLED, IEnumExplorerCommand, IExplorerCommand,
            IExplorerCommand_Impl, IShellItemArray, SIGDN_FILESYSPATH,
        },
    },
    core::{GUID, HRESULT, Interface, PWSTR, Ref, implement},
};

pub const SHELL_COMMAND_CLSID: GUID = GUID::from_u128(0x784bd1f8_26bd_4be8_945f_31e75c6e91a4);
pub const SHELL_COMMAND_CLSID_TEXT: &str = "{784BD1F8-26BD-4BE8-945F-31E75C6E91A4}";

const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
static LAST_ACTIVITY: AtomicU64 = AtomicU64::new(0);

#[implement(IExplorerCommand)]
struct ClipItExplorerCommand;

#[allow(non_snake_case)]
impl IExplorerCommand_Impl for ClipItExplorerCommand_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        touch();
        task_mem_string("使用 ClipIt 发送")
    }

    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        touch();
        let executable = std::env::current_exe().map_err(win_error)?;
        task_mem_string(&format!("{},0", executable.display()))
    }

    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        touch();
        task_mem_string("通过局域网发送所选文件或文件夹")
    }

    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        touch();
        Ok(SHELL_COMMAND_CLSID)
    }

    fn GetState(
        &self,
        items: Ref<'_, IShellItemArray>,
        _ok_to_be_slow: BOOL,
    ) -> windows::core::Result<u32> {
        touch();
        let enabled = items
            .as_ref()
            .is_some_and(|items| unsafe { items.GetCount().is_ok_and(|count| count > 0) });
        Ok(if enabled {
            ECS_ENABLED.0 as u32
        } else {
            ECS_DISABLED.0 as u32
        })
    }

    fn Invoke(
        &self,
        items: Ref<'_, IShellItemArray>,
        _bind_context: Ref<'_, IBindCtx>,
    ) -> windows::core::Result<()> {
        touch();
        let paths = shell_item_paths(items.ok()?)?;
        if paths.is_empty() {
            return Err(windows::core::Error::new(E_FAIL, "没有可发送的文件"));
        }
        let executable = std::env::current_exe().map_err(win_error)?;
        let mut command = Command::new(executable);
        command.arg("pick").args(paths);
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
        command.spawn().map_err(win_error)?;
        Ok(())
    }

    fn GetFlags(&self) -> windows::core::Result<u32> {
        touch();
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct ClipItClassFactory;

#[allow(non_snake_case)]
impl IClassFactory_Impl for ClipItClassFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, windows::core::IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        touch();
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if iid.is_null() || object.is_null() {
            return Err(windows::core::Error::from_hresult(HRESULT(
                0x8000_4003_u32 as i32,
            )));
        }
        let command: IExplorerCommand = ClipItExplorerCommand.into();
        unsafe { command.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        touch();
        Ok(())
    }
}

pub fn run_server() -> Result<()> {
    touch();
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("初始化 Windows COM 失败")?;
    let factory: IClassFactory = ClipItClassFactory.into();
    let cookie = unsafe {
        CoRegisterClassObject(
            &SHELL_COMMAND_CLSID,
            &factory,
            CLSCTX_LOCAL_SERVER,
            REGCLS_MULTIPLEUSE,
        )
    }
    .context("注册 ClipIt ExplorerCommand COM 服务失败")?;

    while now_secs().saturating_sub(LAST_ACTIVITY.load(Ordering::Relaxed)) < IDLE_TIMEOUT.as_secs()
    {
        thread::sleep(Duration::from_secs(1));
    }

    unsafe {
        CoRevokeClassObject(cookie).ok();
        CoUninitialize();
    }
    Ok(())
}

fn shell_item_paths(items: &IShellItemArray) -> windows::core::Result<Vec<PathBuf>> {
    let count = unsafe { items.GetCount()? };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let item = unsafe { items.GetItemAt(index)? };
        let display_name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? };
        let value = unsafe { display_name.to_string() };
        unsafe { CoTaskMemFree(Some(display_name.0.cast())) };
        let value = value?;
        if !value.is_empty() {
            paths.push(PathBuf::from(value));
        }
    }
    Ok(paths)
}

fn task_mem_string(value: &str) -> windows::core::Result<PWSTR> {
    let wide = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let bytes = wide.len().saturating_mul(std::mem::size_of::<u16>());
    let pointer = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
    if pointer.is_null() {
        return Err(windows::core::Error::from_hresult(HRESULT(
            0x8007_000e_u32 as i32,
        )));
    }
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer, wide.len()) };
    Ok(PWSTR(pointer))
}

fn touch() {
    LAST_ACTIVITY.store(now_secs(), Ordering::Relaxed);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn win_error(error: impl std::fmt::Display) -> windows::core::Error {
    windows::core::Error::new(E_FAIL, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_clsid_is_stable() {
        assert_eq!(
            SHELL_COMMAND_CLSID.to_u128(),
            0x784bd1f8_26bd_4be8_945f_31e75c6e91a4
        );
        assert_eq!(
            SHELL_COMMAND_CLSID_TEXT,
            "{784BD1F8-26BD-4BE8-945F-31E75C6E91A4}"
        );
    }
}
