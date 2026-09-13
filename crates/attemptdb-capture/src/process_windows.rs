//! Spawn the scoped runtime without inheriting installer pipeline handles.
//! Redirecting only stdin/stdout/stderr is insufficient: Rust's Command
//! inherits other inheritable handles too, keeping PowerShell's pipe open.
use crate::{DbSource, Locator, service::is_portable};
use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt, path::Path};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    },
};

fn quoted(value: &OsStr) -> Vec<u16> {
    let mut out = vec![34];
    let mut slashes = 0;
    for unit in value.encode_wide() {
        if unit == 92 {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            92,
            if unit == 34 { 2 * slashes + 1 } else { slashes },
        ));
        slashes = 0;
        out.push(unit);
    }
    out.extend(std::iter::repeat_n(92, 2 * slashes));
    out.push(34);
    out
}

pub(crate) fn spawn_daemon(locator: &Locator, binary: &Path) -> io::Result<()> {
    let binary = crate::platform::canonical_display_path(binary);
    let mut arguments = Vec::new();
    if is_portable(&locator.paths) {
        arguments.extend([OsStr::new("--data-dir"), locator.paths.data_dir.as_os_str()]);
    }
    if locator.source != DbSource::Default {
        arguments.extend([OsStr::new("--db"), locator.db_dir.as_os_str()]);
    }
    arguments.extend([OsStr::new("daemon"), OsStr::new("run")]);
    let mut application: Vec<_> = binary.as_os_str().encode_wide().collect();
    let mut command = quoted(binary.as_os_str());
    for argument in arguments {
        command.push(32);
        command.extend(quoted(argument));
    }
    if application.contains(&0) || command.contains(&0) || command.len() >= 32767 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    application.push(0);
    command.push(0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: both UTF-16 strings are terminated and live for this call;
    // command is writable. Default security/environment/current directory
    // are intentional. No handles or console are inherited. The returned
    // handles are closed immediately; closing them does not stop the child.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateProcessW returned owned process/thread handles.
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_paths_keep_spaces_unicode_and_trailing_backslashes() {
        assert_eq!(
            String::from_utf16(&quoted(OsStr::new("C:\\agent work\\한글\\"))).unwrap(),
            "\"C:\\agent work\\한글\\\\\""
        );
        assert_eq!(
            String::from_utf16(&quoted(OsStr::new("a\"b"))).unwrap(),
            "\"a\\\"b\""
        );
    }
}
