//! Windows unelevated 沙箱：受限 Primary Token + capability SID + Workspace ACL。
//!
//! 这与 Codex 的兼容后端属于同一类技术路线。它强制限制文件写入，但断网仅由环境变量
//! 兼容层提供，因此上层状态必须如实标注，不能声称具备 WFP 防火墙隔离。

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GetLastError, HANDLE, HLOCAL, LUID, LocalFree, SetLastError,
};
use windows_sys::Win32::Security::Authorization::{
    DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetExplicitEntriesFromAclW,
    GetNamedSecurityInfoW, SE_WINDOW_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
    SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, AdjustTokenPrivileges, CopySid, CreateRestrictedToken, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, EqualSid, GetLengthSid, GetTokenInformation, LookupPrivilegeValueW,
    SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TokenDefaultDacl, TokenGroups, WRITE_RESTRICTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_DELETE,
    DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD,
    DESKTOP_READ_CONTROL, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_WRITE_DAC,
    DESKTOP_WRITE_OWNER, DESKTOP_WRITEOBJECTS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, WaitForSingleObject,
};

use crate::{SandboxError, SandboxMode};

const DISABLE_MAX_PRIVILEGE: u32 = 0x01;
const CONTAINER_INHERIT_ACE: u32 = 0x2;
const OBJECT_INHERIT_ACE: u32 = 0x1;
const SE_FILE_OBJECT: i32 = 1;
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
const WIN_WORLD_SID: i32 = 1;
const DESKTOP_PARTICIPANT_ACCESS: u32 = DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_HOOKCONTROL
    | DESKTOP_ENUMERATE
    | DESKTOP_WRITEOBJECTS
    | DESKTOP_SWITCHDESKTOP;
const DESKTOP_ALL_ACCESS: u32 = DESKTOP_PARTICIPANT_ACCESS
    | DESKTOP_CREATEMENU
    | DESKTOP_JOURNALRECORD
    | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_DELETE
    | DESKTOP_READ_CONTROL
    | DESKTOP_WRITE_DAC
    | DESKTOP_WRITE_OWNER;
const GENERIC_ALL: u32 = 0x1000_0000;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;

#[repr(C)]
struct TokenDefaultDaclInfo {
    default_dacl: *mut ACL,
}

struct LocalSid(*mut c_void);

impl LocalSid {
    fn from_string(value: &str) -> Result<Self, SandboxError> {
        let mut sid = ptr::null_mut();
        let wide = wide(value);
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
        if ok == 0 || sid.is_null() {
            return Err(last_error("sandbox-sid-convert"));
        }
        Ok(Self(sid))
    }

    const fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn into_raw(mut self) -> HANDLE {
        let handle = self.0;
        self.0 = ptr::null_mut();
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

pub(super) fn run_helper(arguments: Vec<OsString>) -> Result<i32, SandboxError> {
    let parsed = HelperArguments::parse(arguments)?;
    let capability_sid_text = workspace_capability_sid(&parsed.root, parsed.mode);
    let capability_sid = LocalSid::from_string(&capability_sid_text)?;
    match parsed.mode {
        SandboxMode::WorkspaceWrite => {
            set_path_access(&parsed.root, capability_sid.as_ptr(), GRANT_ACCESS)?;
            for protected in [parsed.root.join(".git"), parsed.root.join(".harness")] {
                if protected.exists() {
                    set_path_access(&protected, capability_sid.as_ptr(), DENY_ACCESS)?;
                }
            }
        }
        SandboxMode::ReadOnly => {}
        SandboxMode::DangerFullAccess => {
            return Err(SandboxError::new(
                "sandbox-helper-danger-invalid",
                "Danger full access 不应进入受限 launcher",
            ));
        }
    }
    prepare_isolated_temp(&capability_sid_text, capability_sid.as_ptr())?;
    let token = create_restricted_token(capability_sid.as_ptr())?;
    let result = spawn_and_wait(token, &parsed.command, &parsed.cwd);
    unsafe {
        CloseHandle(token);
    }
    result
}

fn prepare_isolated_temp(sid: &str, sid_pointer: *mut c_void) -> Result<(), SandboxError> {
    let path = std::env::temp_dir().join("kernary-sandbox").join(sid);
    std::fs::create_dir_all(&path)
        .map_err(|error| SandboxError::new("sandbox-temp-create", error.to_string()))?;
    set_path_access(&path, sid_pointer, GRANT_ACCESS)?;
    // 内部 helper 在任何线程创建前执行；目标进程随后继承这两个值。
    unsafe {
        std::env::set_var("TEMP", &path);
        std::env::set_var("TMP", &path);
    }
    Ok(())
}

struct HelperArguments {
    mode: SandboxMode,
    root: PathBuf,
    cwd: PathBuf,
    command: Vec<OsString>,
}

impl HelperArguments {
    fn parse(arguments: Vec<OsString>) -> Result<Self, SandboxError> {
        let separator = arguments
            .iter()
            .position(|value| value == "--")
            .ok_or_else(|| SandboxError::new("sandbox-helper-args", "缺少 -- 分隔符"))?;
        let options = &arguments[..separator];
        let command = arguments[separator + 1..].to_vec();
        if command.is_empty() {
            return Err(SandboxError::new("sandbox-helper-args", "缺少目标命令"));
        }
        let mut mode = None;
        let mut root = None;
        let mut cwd = None;
        let mut index = 0;
        while index < options.len() {
            let key = options[index].to_string_lossy();
            let value = options
                .get(index + 1)
                .ok_or_else(|| SandboxError::new("sandbox-helper-args", format!("{key} 缺少值")))?;
            match key.as_ref() {
                "--mode" => mode = Some(SandboxMode::parse(&value.to_string_lossy())?),
                "--root" => root = Some(PathBuf::from(value)),
                "--cwd" => cwd = Some(PathBuf::from(value)),
                _ => {
                    return Err(SandboxError::new(
                        "sandbox-helper-args",
                        format!("未知参数：{key}"),
                    ));
                }
            }
            index += 2;
        }
        let root = std::fs::canonicalize(
            root.ok_or_else(|| SandboxError::new("sandbox-helper-args", "缺少 --root"))?,
        )
        .map_err(|error| SandboxError::new("sandbox-root-canonicalize", error.to_string()))?;
        let cwd = std::fs::canonicalize(
            cwd.ok_or_else(|| SandboxError::new("sandbox-helper-args", "缺少 --cwd"))?,
        )
        .map_err(|error| SandboxError::new("sandbox-cwd-canonicalize", error.to_string()))?;
        if !cwd.starts_with(&root) {
            return Err(SandboxError::new(
                "sandbox-cwd-escape",
                cwd.display().to_string(),
            ));
        }
        Ok(Self {
            mode: mode.ok_or_else(|| SandboxError::new("sandbox-helper-args", "缺少 --mode"))?,
            root,
            cwd,
            command,
        })
    }
}

fn workspace_capability_sid(root: &Path, mode: SandboxMode) -> String {
    let normalized = format!("{}|{mode}", root.to_string_lossy().to_ascii_lowercase());
    let digest = Sha256::digest(normalized.as_bytes());
    let part = |offset: usize| {
        u32::from_le_bytes([
            digest[offset],
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ])
    };
    format!("S-1-5-21-{}-{}-{}-{}", part(0), part(4), part(8), part(12))
}

fn create_restricted_token(capability_sid: *mut c_void) -> Result<HANDLE, SandboxError> {
    let mut base_raw: HANDLE = ptr::null_mut();
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID
                | TOKEN_ADJUST_PRIVILEGES,
            &mut base_raw,
        )
    };
    if opened == 0 {
        return Err(last_error("sandbox-token-open"));
    }
    let base = OwnedHandle(base_raw);
    let mut logon_sid = token_logon_sid(base.0)?;
    let mut world_sid = world_sid()?;
    let mut restricting = [
        SID_AND_ATTRIBUTES {
            Sid: capability_sid,
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: logon_sid.as_mut_ptr() as *mut c_void,
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: world_sid.as_mut_ptr() as *mut c_void,
            Attributes: 0,
        },
    ];
    let mut token: HANDLE = ptr::null_mut();
    let created = unsafe {
        CreateRestrictedToken(
            base.0,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            0,
            ptr::null(),
            0,
            ptr::null(),
            restricting.len() as u32,
            restricting.as_mut_ptr(),
            &mut token,
        )
    };
    if created == 0 {
        return Err(last_error("sandbox-token-create"));
    }
    let token = OwnedHandle(token);
    set_default_dacl(
        token.0,
        &[
            capability_sid,
            logon_sid.as_mut_ptr() as *mut c_void,
            world_sid.as_mut_ptr() as *mut c_void,
        ],
    )?;
    enable_change_notify_privilege(token.0)?;
    Ok(token.into_raw())
}

fn set_default_dacl(token: HANDLE, sids: &[*mut c_void]) -> Result<(), SandboxError> {
    let entries = sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: *sid as *mut u16,
            },
        })
        .collect::<Vec<_>>();
    let mut dacl: *mut ACL = ptr::null_mut();
    let merged = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            ptr::null_mut(),
            &mut dacl,
        )
    };
    if merged != ERROR_SUCCESS {
        return Err(SandboxError::new(
            "sandbox-token-dacl-merge",
            format!("Windows error {merged}"),
        ));
    }
    let mut info = TokenDefaultDaclInfo { default_dacl: dacl };
    let applied = unsafe {
        SetTokenInformation(
            token,
            TokenDefaultDacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<TokenDefaultDaclInfo>() as u32,
        )
    };
    let error = unsafe { GetLastError() };
    unsafe { LocalFree(dacl as HLOCAL) };
    if applied == 0 {
        return Err(SandboxError::new(
            "sandbox-token-dacl-apply",
            format!("Windows error {error}"),
        ));
    }
    Ok(())
}

fn enable_change_notify_privilege(token: HANDLE) -> Result<(), SandboxError> {
    let mut luid: LUID = unsafe { std::mem::zeroed() };
    let name = wide("SeChangeNotifyPrivilege");
    let found = unsafe { LookupPrivilegeValueW(ptr::null(), name.as_ptr(), &mut luid) };
    if found == 0 {
        return Err(last_error("sandbox-token-lookup-privilege"));
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [windows_sys::Win32::Security::LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: 0x0000_0002,
        }],
    };
    unsafe { SetLastError(ERROR_SUCCESS) };
    let adjusted = unsafe {
        AdjustTokenPrivileges(token, 0, &privileges, 0, ptr::null_mut(), ptr::null_mut())
    };
    if adjusted == 0 {
        return Err(last_error("sandbox-token-enable-privilege"));
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_SUCCESS {
        return Err(SandboxError::new(
            "sandbox-token-privilege-not-assigned",
            format!("Windows error {error}"),
        ));
    }
    Ok(())
}

fn world_sid() -> Result<Vec<u8>, SandboxError> {
    let mut needed = 0_u32;
    unsafe {
        CreateWellKnownSid(WIN_WORLD_SID, ptr::null_mut(), ptr::null_mut(), &mut needed);
    }
    if needed == 0 {
        return Err(last_error("sandbox-world-sid-size"));
    }
    let mut sid = vec![0_u8; needed as usize];
    let created = unsafe {
        CreateWellKnownSid(
            WIN_WORLD_SID,
            ptr::null_mut(),
            sid.as_mut_ptr() as *mut c_void,
            &mut needed,
        )
    };
    if created == 0 {
        return Err(last_error("sandbox-world-sid"));
    }
    Ok(sid)
}

fn token_logon_sid(token: HANDLE) -> Result<Vec<u8>, SandboxError> {
    let mut needed = 0_u32;
    unsafe {
        GetTokenInformation(token, TokenGroups, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(last_error("sandbox-token-groups-size"));
    }
    let mut buffer = vec![0_u8; needed as usize];
    let loaded = unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            buffer.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        )
    };
    if loaded == 0 {
        return Err(last_error("sandbox-token-groups"));
    }
    let group_count = unsafe { ptr::read_unaligned(buffer.as_ptr() as *const u32) } as usize;
    let after_count = unsafe { buffer.as_ptr().add(std::mem::size_of::<u32>()) } as usize;
    let alignment = std::mem::align_of::<SID_AND_ATTRIBUTES>();
    let aligned = (after_count + alignment - 1) & !(alignment - 1);
    let groups = aligned as *const SID_AND_ATTRIBUTES;
    for index in 0..group_count {
        let group = unsafe { ptr::read_unaligned(groups.add(index)) };
        if group.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID {
            let length = unsafe { GetLengthSid(group.Sid) };
            if length == 0 {
                break;
            }
            let mut sid = vec![0_u8; length as usize];
            let copied = unsafe { CopySid(length, sid.as_mut_ptr() as *mut c_void, group.Sid) };
            if copied != 0 {
                return Ok(sid);
            }
        }
    }
    Err(SandboxError::new(
        "sandbox-token-logon-sid",
        "当前 Token 缺少 Logon SID",
    ))
}

fn set_path_access(path: &Path, sid: *mut c_void, mode: i32) -> Result<(), SandboxError> {
    let mut security_descriptor = ptr::null_mut();
    let mut old_dacl: *mut ACL = ptr::null_mut();
    let mut path_wide = wide(path.as_os_str());
    let result = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut old_dacl,
            ptr::null_mut(),
            &mut security_descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(SandboxError::new(
            "sandbox-acl-read",
            format!("{}: Windows error {result}", path.display()),
        ));
    }

    if dacl_has_access(old_dacl, sid, mode) {
        unsafe {
            LocalFree(security_descriptor as HLOCAL);
        }
        return Ok(());
    }

    let write_mask = FILE_GENERIC_READ
        | FILE_GENERIC_WRITE
        | FILE_GENERIC_EXECUTE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD;
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: write_mask,
        grfAccessMode: mode,
        grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        },
    };
    let mut new_dacl: *mut ACL = ptr::null_mut();
    let merge = unsafe { SetEntriesInAclW(1, &access, old_dacl, &mut new_dacl) };
    if merge != ERROR_SUCCESS {
        unsafe {
            LocalFree(security_descriptor as HLOCAL);
        }
        return Err(SandboxError::new(
            "sandbox-acl-merge",
            format!("{}: Windows error {merge}", path.display()),
        ));
    }
    let applied = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            new_dacl,
            ptr::null_mut(),
        )
    };
    unsafe {
        LocalFree(new_dacl as HLOCAL);
        LocalFree(security_descriptor as HLOCAL);
    }
    if applied != ERROR_SUCCESS {
        return Err(SandboxError::new(
            "sandbox-acl-apply",
            format!("{}: Windows error {applied}", path.display()),
        ));
    }
    Ok(())
}

fn dacl_has_access(dacl: *mut ACL, sid: *mut c_void, mode: i32) -> bool {
    if dacl.is_null() {
        return false;
    }
    let mut count = 0_u32;
    let mut entries: *mut EXPLICIT_ACCESS_W = ptr::null_mut();
    let result = unsafe { GetExplicitEntriesFromAclW(dacl, &mut count, &mut entries) };
    if result != ERROR_SUCCESS || entries.is_null() {
        return false;
    }
    let found = (0..count as usize).any(|index| {
        let entry = unsafe { &*entries.add(index) };
        let matching_mode = if mode == DENY_ACCESS {
            entry.grfAccessMode == DENY_ACCESS
        } else {
            matches!(entry.grfAccessMode, GRANT_ACCESS | SET_ACCESS)
        };
        matching_mode
            && entry.Trustee.TrusteeForm == TRUSTEE_IS_SID
            && unsafe { EqualSid(entry.Trustee.ptstrName as *mut c_void, sid) } != 0
            && entry.grfAccessPermissions & FILE_WRITE_DATA != 0
    });
    unsafe {
        LocalFree(entries as HLOCAL);
    }
    found
}

fn spawn_and_wait(token: HANDLE, command: &[OsString], cwd: &Path) -> Result<i32, SandboxError> {
    let desktop = PrivateDesktop::create(token)?;
    let command_line = windows_command_line(command);
    let mut command_wide = wide(command_line);
    // std::fs::canonicalize 会返回 `\\?\` 路径；CMD 等传统程序把它误判为 UNC cwd。
    let launch_cwd = non_verbatim_path(cwd);
    let cwd_wide = wide(launch_cwd.as_os_str());
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.lpDesktop = desktop.startup_name.as_ptr() as *mut u16;
    startup.hStdInput = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    startup.hStdOutput = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    startup.hStdError = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessAsUserW(
            token,
            ptr::null(),
            command_wide.as_mut_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            1,
            CREATE_NO_WINDOW,
            ptr::null_mut(),
            cwd_wide.as_ptr(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(last_error("sandbox-process-create"));
    }
    unsafe { CloseHandle(process.hThread) };
    let waited = unsafe { WaitForSingleObject(process.hProcess, INFINITE) };
    if waited == WAIT_FAILED {
        let error = last_error("sandbox-process-wait");
        unsafe { CloseHandle(process.hProcess) };
        return Err(error);
    }
    let mut exit_code = 1_u32;
    let exit_ok = unsafe { GetExitCodeProcess(process.hProcess, &mut exit_code) };
    unsafe {
        CloseHandle(process.hProcess);
    }
    if exit_ok == 0 {
        return Err(last_error("sandbox-process-exit"));
    }
    Ok(exit_code as i32)
}

struct PrivateDesktop {
    handle: *mut c_void,
    startup_name: Vec<u16>,
}

impl PrivateDesktop {
    fn create(token: HANDLE) -> Result<Self, SandboxError> {
        let name = format!(
            "KernarySandboxDesktop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        );
        let name_wide = wide(&name);
        let handle = unsafe {
            CreateDesktopW(
                name_wide.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                0,
                DESKTOP_ALL_ACCESS,
                ptr::null_mut(),
            )
        };
        if handle.is_null() {
            return Err(last_error("sandbox-desktop-create"));
        }
        let desktop = Self {
            handle,
            startup_name: wide(format!("Winsta0\\{name}")),
        };
        let mut logon_sid = token_logon_sid(token)?;
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: DESKTOP_PARTICIPANT_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: logon_sid.as_mut_ptr() as *mut u16,
            },
        };
        let mut dacl: *mut ACL = ptr::null_mut();
        let merged = unsafe { SetEntriesInAclW(1, &access, ptr::null_mut(), &mut dacl) };
        if merged != ERROR_SUCCESS {
            unsafe {
                CloseDesktop(desktop.handle);
            }
            std::mem::forget(desktop);
            return Err(SandboxError::new(
                "sandbox-desktop-acl-merge",
                format!("Windows error {merged}"),
            ));
        }
        let applied = unsafe {
            SetSecurityInfo(
                handle,
                SE_WINDOW_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null_mut(),
            )
        };
        unsafe {
            LocalFree(dacl as HLOCAL);
        }
        if applied != ERROR_SUCCESS {
            unsafe {
                CloseDesktop(desktop.handle);
            }
            std::mem::forget(desktop);
            return Err(SandboxError::new(
                "sandbox-desktop-acl-apply",
                format!("Windows error {applied}"),
            ));
        }
        Ok(desktop)
    }
}

impl Drop for PrivateDesktop {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseDesktop(self.handle);
            }
        }
    }
}

fn windows_command_line(arguments: &[OsString]) -> String {
    arguments
        .iter()
        .map(|argument| quote_windows_argument(&argument.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn non_verbatim_path(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    value
        .strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
        } else {
            if character == '"' {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            } else {
                quoted.push_str(&"\\".repeat(backslashes));
            }
            backslashes = 0;
            quoted.push(character);
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn last_error(code: &str) -> SandboxError {
    SandboxError::new(code, format!("Windows error {}", unsafe { GetLastError() }))
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSidToSidW(string_sid: *const u16, sid: *mut *mut c_void) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::IsTokenRestricted;

    #[test]
    fn quoting_preserves_spaces_quotes_and_trailing_slashes() {
        assert_eq!(quote_windows_argument("plain"), "plain");
        assert_eq!(quote_windows_argument("a b"), "\"a b\"");
        assert_eq!(quote_windows_argument(""), "\"\"");
        assert_eq!(quote_windows_argument("a\\"), "a\\");
        assert_eq!(quote_windows_argument("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn workspace_sid_is_stable_and_path_specific() {
        let first =
            workspace_capability_sid(Path::new(r"C:\\work\\a"), SandboxMode::WorkspaceWrite);
        assert_eq!(
            first,
            workspace_capability_sid(Path::new(r"c:\\WORK\\a"), SandboxMode::WorkspaceWrite)
        );
        assert_ne!(
            first,
            workspace_capability_sid(Path::new(r"C:\\work\\b"), SandboxMode::WorkspaceWrite)
        );
        assert_ne!(
            first,
            workspace_capability_sid(Path::new(r"C:\\work\\a"), SandboxMode::ReadOnly)
        );
    }

    #[test]
    fn restricted_token_enforces_read_only_workspace_and_escape_boundaries() {
        // Codex 自身的 Windows 沙箱 Token 不能再次添加任意 restricting SID；正常用户和 CI 执行完整验收。
        let mut token: HANDLE = ptr::null_mut();
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        assert_ne!(opened, 0, "open current token");
        let already_restricted = unsafe { IsTokenRestricted(token) } != 0;
        unsafe {
            CloseHandle(token);
        }
        if already_restricted {
            return;
        }

        let parent = tempfile::tempdir().expect("sandbox parent");
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).expect("workspace");
        let inside = root.join("inside.txt");
        let readonly = root.join("readonly.txt");
        let outside = parent.path().join("outside.txt");
        let command = |mode: &str, target: &Path| {
            run_helper(vec![
                OsString::from("--mode"),
                OsString::from(mode),
                OsString::from("--root"),
                root.as_os_str().to_owned(),
                OsString::from("--cwd"),
                root.as_os_str().to_owned(),
                OsString::from("--"),
                OsString::from(r"C:\Windows\System32\cmd.exe"),
                OsString::from("/D"),
                OsString::from("/S"),
                OsString::from("/C"),
                OsString::from(format!("echo test>{}", target.display())),
            ])
            .expect("sandbox helper")
        };

        assert_eq!(command("workspace-write", &inside), 0);
        assert!(inside.is_file());
        assert_ne!(command("workspace-write", &outside), 0);
        assert!(!outside.exists());
        assert_ne!(command("read-only", &readonly), 0);
        assert!(!readonly.exists());
    }
}
