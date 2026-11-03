//! Current-user, interactive, passwordless elevated logon startup.
use std::{io, path::Path};
use windows::Win32::{
    Foundation::{
        CloseHandle, HANDLE, HLOCAL, LocalFree, RPC_E_CHANGED_MODE, SYSTEMTIME, VARIANT_BOOL,
    },
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, LookupAccountNameW, PSID,
        SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        TaskScheduler::{
            IExecAction, ILogonTrigger, IRegisteredTask, ITaskDefinition, ITaskFolder,
            ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
            TASK_RUNLEVEL_HIGHEST, TASK_TRIGGER_LOGON, TaskScheduler,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
        Variant::{VARIANT, VariantTimeToSystemTime},
    },
};
use windows_core::{BSTR, Interface, PCWSTR, PWSTR};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskState {
    Missing,
    Disabled,
    Valid,
    Stale,
}

struct Apartment(bool);
impl Apartment {
    fn enter() -> windows_core::Result<Self> {
        // SAFETY: balanced only when this call initialized COM; an existing MTA is usable too.
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        result.ok()?;
        Ok(Self(true))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

pub(crate) fn current_sid() -> windows_core::Result<String> {
    // SAFETY: query-only process token; aligned bounded buffer; all native allocations released.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token)?;
        let result = (|| {
            let mut required = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &raw mut required);
            if required == 0 || required > 65536 {
                return Err(windows_core::Error::from_thread());
            }
            let mut data = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
            GetTokenInformation(
                token,
                TokenUser,
                Some(data.as_mut_ptr().cast()),
                required,
                &raw mut required,
            )?;
            let user = &*data.as_ptr().cast::<TOKEN_USER>();
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &raw mut text)?;
            let result = text.to_string().map_err(windows_core::Error::from);
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            result
        })();
        let _ = CloseHandle(token);
        result
    }
}

fn folder() -> windows_core::Result<ITaskFolder> {
    // SAFETY: initialized COM, local scheduler and empty credentials use current token only.
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let empty = VARIANT::default();
        service.Connect(&empty, &empty, &empty, &empty)?;
        service.GetFolder(&BSTR::from("\\"))
    }
}

fn same_account(account: &BSTR, sid: &str) -> windows_core::Result<bool> {
    if account == sid {
        return Ok(true);
    }
    // Scheduler may canonicalize a SID into a localized account name. Resolve it back;
    // comparing display names would fail on normal and Chinese Windows accounts.
    let mut buffer = [0_usize; 128];
    let mut bytes = 1024;
    let mut domain = [0_u16; 512];
    let mut domain_len = 512;
    let mut usage = SID_NAME_USE::default();
    unsafe {
        let native_sid = PSID(buffer.as_mut_ptr().cast());
        LookupAccountNameW(
            None,
            PCWSTR(account.as_ptr()),
            Some(native_sid),
            &raw mut bytes,
            Some(PWSTR(domain.as_mut_ptr())),
            &raw mut domain_len,
            &raw mut usage,
        )?;
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(native_sid, &raw mut text)?;
        let resolved = text.to_string();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        Ok(resolved? == sid)
    }
}

fn name(sid: &str) -> BSTR {
    BSTR::from(format!("ClipFerry Logon {sid}"))
}

fn lookup(folder: &ITaskFolder, sid: &str) -> windows_core::Result<Option<IRegisteredTask>> {
    match unsafe { folder.GetTask(&name(sid)) } {
        Ok(task) => Ok(Some(task)),
        Err(e) if e.code() == windows_core::HRESULT::from_win32(2) => Ok(None),
        Err(e) => Err(e),
    }
}

fn inspect(task: &IRegisteredTask, sid: &str, exe: &Path) -> windows_core::Result<TaskState> {
    // SAFETY: typed COM getters return owned interfaces; out parameters have matching types.
    unsafe {
        if !task.Enabled()?.as_bool() {
            return Ok(TaskState::Disabled);
        }
        let definition = task.Definition()?;
        inspect_definition(&definition, sid, exe)
    }
}

fn inspect_definition(
    definition: &ITaskDefinition,
    sid: &str,
    exe: &Path,
) -> windows_core::Result<TaskState> {
    // SAFETY: owned COM interfaces and correctly typed out parameters throughout.
    unsafe {
        let settings = definition.Settings()?;
        let mut on_battery = VARIANT_BOOL::default();
        let mut stop_battery = VARIANT_BOOL::default();
        let mut network = VARIANT_BOOL::default();
        let mut limit = BSTR::new();
        settings.DisallowStartIfOnBatteries(&raw mut on_battery)?;
        settings.StopIfGoingOnBatteries(&raw mut stop_battery)?;
        settings.RunOnlyIfNetworkAvailable(&raw mut network)?;
        settings.ExecutionTimeLimit(&raw mut limit)?;
        if on_battery.as_bool() || stop_battery.as_bool() || network.as_bool() || limit != "PT0S" {
            return Ok(TaskState::Stale);
        }
        let principal = definition.Principal()?;
        let mut user = BSTR::new();
        let mut level = TASK_RUNLEVEL_HIGHEST;
        let mut logon = TASK_LOGON_INTERACTIVE_TOKEN;
        principal.UserId(&raw mut user)?;
        principal.RunLevel(&raw mut level)?;
        principal.LogonType(&raw mut logon)?;
        if !same_account(&user, sid)?
            || level != TASK_RUNLEVEL_HIGHEST
            || logon != TASK_LOGON_INTERACTIVE_TOKEN
        {
            return Ok(TaskState::Stale);
        }
        let actions = definition.Actions()?;
        let triggers = definition.Triggers()?;
        let mut count = 0;
        actions.Count(&raw mut count)?;
        if count != 1 {
            return Ok(TaskState::Stale);
        }
        triggers.Count(&raw mut count)?;
        if count != 1 {
            return Ok(TaskState::Stale);
        }
        let action = actions.get_Item(1)?;
        let Ok(action) = action.cast::<IExecAction>() else {
            return Ok(TaskState::Stale);
        };
        let mut path = BSTR::new();
        let mut arguments = BSTR::new();
        action.Path(&raw mut path)?;
        action.Arguments(&raw mut arguments)?;
        let trigger = triggers.get_Item(1)?;
        let mut kind = TASK_TRIGGER_LOGON;
        let mut enabled = VARIANT_BOOL::default();
        trigger.Type(&raw mut kind)?;
        trigger.Enabled(&raw mut enabled)?;
        if kind != TASK_TRIGGER_LOGON || !enabled.as_bool() {
            return Ok(TaskState::Stale);
        }
        let trigger: ILogonTrigger = trigger.cast()?;
        let mut trigger_user = BSTR::new();
        trigger.UserId(&raw mut trigger_user)?;
        Ok(
            if path
                .to_string()
                .eq_ignore_ascii_case(&exe.to_string_lossy())
                && arguments == "tray"
                && same_account(&trigger_user, sid)?
            {
                TaskState::Valid
            } else {
                TaskState::Stale
            },
        )
    }
}

pub(crate) fn query(exe: &Path) -> io::Result<TaskState> {
    (|| -> windows_core::Result<TaskState> {
        let _apartment = Apartment::enter()?;
        let sid = current_sid()?;
        match lookup(&folder()?, &sid)? {
            Some(task) => inspect(&task, &sid, exe),
            None => Ok(TaskState::Missing),
        }
    })()
    .map_err(io::Error::other)
}

/// Outcome of the most recent scheduler-triggered run of the logon task.
pub(crate) struct LastRun {
    pub(crate) time: Option<String>,
    pub(crate) result: i32,
}

const SCHED_S_TASK_RUNNING: i32 = 0x0004_1301;
const SCHED_S_TASK_HAS_NOT_RUN: i32 = 0x0004_1303;

pub(crate) fn result_label(code: i32) -> String {
    match code {
        0 => "ok".into(),
        SCHED_S_TASK_HAS_NOT_RUN => "never".into(),
        SCHED_S_TASK_RUNNING => "running".into(),
        other => format!("failed:{other:#010x}"),
    }
}

fn format_run_time(ole_date: f64) -> Option<String> {
    if !ole_date.is_finite() || ole_date < 1.0 {
        return None;
    }
    let mut st = SYSTEMTIME::default();
    // SAFETY: valid out pointer to a stack SYSTEMTIME.
    if unsafe { VariantTimeToSystemTime(ole_date, &raw mut st) } == 0 || st.wYear < 2000 {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    ))
}

/// Read-only: `None` when the task does not exist.
pub(crate) fn last_run() -> io::Result<Option<LastRun>> {
    (|| -> windows_core::Result<Option<LastRun>> {
        let _apartment = Apartment::enter()?;
        let sid = current_sid()?;
        let Some(task) = lookup(&folder()?, &sid)? else {
            return Ok(None);
        };
        // SAFETY: typed COM getters on a live task; plain value out parameters.
        let (result, time) = unsafe { (task.LastTaskResult()?, task.LastRunTime()?) };
        Ok(Some(LastRun {
            time: format_run_time(time),
            result,
        }))
    })()
    .map_err(io::Error::other)
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn task_xml(sid: &str, exe: &Path) -> String {
    let sid = xml_escape(sid);
    let path = xml_escape(&exe.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><Description>Pierdesk current-user interactive logon startup</Description></RegistrationInfo>
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
<Principals><Principal id="User"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>false</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
<Actions Context="User"><Exec><Command>{path}</Command><Arguments>tray</Arguments></Exec></Actions></Task>"#
    )
}

pub(crate) fn enable(exe: &Path) -> io::Result<()> {
    (|| -> windows_core::Result<()> {
        let _apartment = Apartment::enter()?;
        let sid = current_sid()?;
        let folder = folder()?;
        let empty = VARIANT::default();
        // No password, SYSTEM account, shell, startup launch or network condition.
        let task = unsafe {
            folder.RegisterTask(
                &name(&sid),
                &BSTR::from(task_xml(&sid, exe)),
                TASK_CREATE_OR_UPDATE.0,
                &VARIANT::from(sid.as_str()),
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )?
        };
        if inspect(&task, &sid, exe)? != TaskState::Valid {
            return Err(windows_core::Error::new(
                windows_core::HRESULT::from_win32(13),
                "Autostart task readback mismatch",
            ));
        }
        Ok(())
    })()
    .map_err(io::Error::other)
}

pub(crate) fn disable() -> io::Result<()> {
    (|| -> windows_core::Result<()> {
        let _apartment = Apartment::enter()?;
        let sid = current_sid()?;
        let folder = folder()?;
        if lookup(&folder, &sid)?.is_some() {
            unsafe {
                folder.DeleteTask(&name(&sid), 0)?;
            }
        }
        Ok(())
    })()
    .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn elevated_logon_is_interactive_scoped_and_not_limited_by_battery_or_runtime() {
        let xml = task_xml(
            "S-1-5-21-123",
            Path::new(r"C:\Program Files\Pierdesk\pierdesk.exe"),
        );
        for expected in [
            "<UserId>S-1-5-21-123</UserId>",
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>HighestAvailable</RunLevel>",
            "<Arguments>tray</Arguments>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<DisallowStartIfOnBatteries>false",
            "<StopIfGoingOnBatteries>false",
            "<RunOnlyIfNetworkAvailable>false",
            "<MultipleInstancesPolicy>IgnoreNew",
        ] {
            assert!(xml.contains(expected), "{expected}");
        }
        assert!(!xml.contains("Password"));
        assert!(!xml.contains("powershell"));
    }
    #[test]
    fn xml_does_not_interpret_paths_as_markup_or_shell() {
        let xml = task_xml(
            "S-1-5-21-123",
            Path::new("C:\\中文 & <files>\\pierdesk.exe"),
        );
        assert!(xml.contains("C:\\中文 &amp; &lt;files&gt;\\pierdesk.exe"));
        assert_eq!(xml.matches("<Exec>").count(), 1);
    }
    #[test]
    fn current_sid_and_scheduler_can_be_read_without_changing_tasks() {
        let _apartment = Apartment::enter().unwrap();
        assert!(current_sid().unwrap().starts_with("S-1-5-"));
        folder().unwrap();
    }

    #[test]
    fn last_run_reads_without_changing_tasks() {
        // Missing task (Ok(None)) is as valid as an existing one.
        let _ = last_run().unwrap();
    }

    #[test]
    fn result_codes_map_to_labels() {
        assert_eq!(result_label(0), "ok");
        assert_eq!(result_label(0x41303), "never");
        assert_eq!(result_label(0x41301), "running");
        assert_eq!(result_label(1), "failed:0x00000001");
        assert_eq!(format_run_time(0.0), None);
    }

    #[test]
    fn windows_accepts_task_xml_without_registering_or_running_it() {
        let _apartment = Apartment::enter().unwrap();
        let sid = current_sid().unwrap();
        unsafe {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).unwrap();
            let empty = VARIANT::default();
            service.Connect(&empty, &empty, &empty, &empty).unwrap();
            let definition = service.NewTask(0).unwrap();
            definition
                .SetXmlText(&BSTR::from(task_xml(
                    &sid,
                    Path::new(r"C:\中文 & Apps\pierdesk.exe"),
                )))
                .unwrap();
            let mut user = BSTR::new();
            definition
                .Principal()
                .unwrap()
                .UserId(&raw mut user)
                .unwrap();
            assert!(same_account(&user, &sid).unwrap());
            let action: IExecAction = definition
                .Actions()
                .unwrap()
                .get_Item(1)
                .unwrap()
                .cast()
                .unwrap();
            let mut path = BSTR::new();
            action.Path(&raw mut path).unwrap();
            assert_eq!(path.to_string(), r"C:\中文 & Apps\pierdesk.exe");
            assert_eq!(
                inspect_definition(
                    &definition,
                    &sid,
                    Path::new(r"C:\中文 & Apps\pierdesk.exe")
                )
                .unwrap(),
                TaskState::Valid
            );
            assert_eq!(
                inspect_definition(&definition, &sid, Path::new(r"C:\other\pierdesk.exe"))
                    .unwrap(),
                TaskState::Stale
            );
            definition
                .Settings()
                .unwrap()
                .SetDisallowStartIfOnBatteries(true.into())
                .unwrap();
            assert_eq!(
                inspect_definition(
                    &definition,
                    &sid,
                    Path::new(r"C:\中文 & Apps\pierdesk.exe")
                )
                .unwrap(),
                TaskState::Stale
            );
        }
    }
}
