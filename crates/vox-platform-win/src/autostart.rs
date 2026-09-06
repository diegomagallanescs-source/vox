//! Start-at-login via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::misc::wide;
use crate::PlatformError;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("Vox");

fn check(context: &'static str, err: WIN32_ERROR) -> Result<(), PlatformError> {
    if err == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(PlatformError::Other(format!(
            "{context}: Win32 error {}",
            err.0
        )))
    }
}

/// Passed by the autostart entry so a login launch stays in the tray instead of opening
/// the settings window.
pub const MINIMIZED_FLAG: &str = "--minimized";

/// Registers (or removes) the current executable to start at login.
pub fn set_autostart(enabled: bool) -> Result<(), PlatformError> {
    let exe =
        std::env::current_exe().map_err(|e| PlatformError::Other(format!("current_exe: {e}")))?;
    unsafe {
        let mut key = HKEY(std::ptr::null_mut());
        check(
            "RegCreateKeyExW(Run)",
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_READ,
                None,
                &mut key,
                None,
            ),
        )?;
        let result = if enabled {
            let value = wide(&format!("\"{}\" {MINIMIZED_FLAG}", exe.display()));
            let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
            check(
                "RegSetValueExW(Vox)",
                RegSetValueExW(key, VALUE, None, REG_SZ, Some(bytes)),
            )
        } else {
            let err = RegDeleteValueW(key, VALUE);
            if err == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                check("RegDeleteValueW(Vox)", err)
            }
        };
        let _ = RegCloseKey(key);
        result
    }
}

/// Whether a `Vox` value currently exists under the Run key (regardless of its path).
pub fn is_autostart_enabled() -> bool {
    unsafe {
        let mut key = HKEY(std::ptr::null_mut());
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ,
            None,
            &mut key,
            None,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let err = RegQueryValueExW(key, VALUE, None, None, None, None);
        let _ = RegCloseKey(key);
        err == ERROR_SUCCESS
    }
}
