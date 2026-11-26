#[cfg(target_os = "windows")]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const HKEY_CURRENT_USER: isize = -2147483647i32 as isize;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_MORE_DATA: i32 = 234;

    const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE_NAME: &str = "FineShot";

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            h_key: isize,
            lp_sub_key: *const u16,
            ul_options: u32,
            sam_desired: u32,
            phk_result: *mut isize,
        ) -> i32;
        fn RegQueryValueExW(
            h_key: isize,
            lp_value_name: *const u16,
            lp_reserved: *mut u32,
            lp_type: *mut u32,
            lp_data: *mut u8,
            lpcb_data: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            h_key: isize,
            lp_value_name: *const u16,
            reserved: u32,
            dw_type: u32,
            lp_data: *const u8,
            cb_data: u32,
        ) -> i32;
        fn RegDeleteValueW(h_key: isize, lp_value_name: *const u16) -> i32;
        fn RegCloseKey(h_key: isize) -> i32;
    }

    fn to_wide_null(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn build_startup_command(exe_path: &Path) -> String {
        format!("\"{}\"", exe_path.display())
    }

    pub(super) fn query_value_exists(value_name: &str) -> Result<bool, i32> {
        let subkey = to_wide_null(RUN_SUBKEY);
        let name = to_wide_null(value_name);
        let mut hkey: isize = 0;
        let open_res = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut hkey,
            )
        };
        if open_res != ERROR_SUCCESS {
            return Err(open_res);
        }

        let mut val_type: u32 = 0;
        let mut cb_data: u32 = 0;
        let q_res = unsafe {
            RegQueryValueExW(
                hkey,
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut val_type,
                std::ptr::null_mut(),
                &mut cb_data,
            )
        };
        unsafe {
            RegCloseKey(hkey);
        }

        match q_res {
            ERROR_SUCCESS | ERROR_MORE_DATA => {
                let is_str = val_type == REG_SZ || val_type == REG_EXPAND_SZ;
                Ok(is_str && cb_data > 2)
            }
            ERROR_FILE_NOT_FOUND => Ok(false),
            err => Err(err),
        }
    }

    pub(super) fn set_value(value_name: &str, command: &str) -> Result<(), String> {
        let subkey = to_wide_null(RUN_SUBKEY);
        let name = to_wide_null(value_name);
        let data = to_wide_null(command);
        let mut hkey: isize = 0;
        let open_res = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut hkey,
            )
        };
        if open_res != ERROR_SUCCESS {
            return Err(format!("Failed to open Startup registry key (err {open_res})"));
        }

        let cb_data = (data.len() * std::mem::size_of::<u16>()) as u32;
        let set_res = unsafe {
            RegSetValueExW(
                hkey,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                cb_data,
            )
        };
        unsafe {
            RegCloseKey(hkey);
        }

        if set_res == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("Failed to write Startup registry value (err {set_res})"))
        }
    }

    pub(super) fn delete_value(value_name: &str) -> Result<(), String> {
        let subkey = to_wide_null(RUN_SUBKEY);
        let name = to_wide_null(value_name);
        let mut hkey: isize = 0;
        let open_res = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut hkey,
            )
        };
        if open_res == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if open_res != ERROR_SUCCESS {
            return Err(format!("Failed to open Startup registry key (err {open_res})"));
        }

        let del_res = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
        unsafe {
            RegCloseKey(hkey);
        }

        if del_res == ERROR_SUCCESS || del_res == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("Failed to remove Startup registry value (err {del_res})"))
        }
    }

    pub fn is_enabled() -> bool {
        query_value_exists(VALUE_NAME).unwrap_or(false)
    }

    pub fn set_enabled(enable: bool) -> Result<(), String> {
        if enable {
            let exe = std::env::current_exe()
                .map_err(|e| format!("Failed to resolve executable path: {e}"))?;
            let cmd = build_startup_command(&exe);
            set_value(VALUE_NAME, &cmd)
        } else {
            delete_value(VALUE_NAME)
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod win {
    pub fn is_enabled() -> bool {
        false
    }

    pub fn set_enabled(_enable: bool) -> Result<(), String> {
        Err("Windows autostart is only supported on Windows".to_string())
    }
}

pub use win::{is_enabled, set_enabled};

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::win::{build_startup_command, delete_value, query_value_exists, set_value};
    use std::path::Path;

    struct CleanupGuard(&'static str);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = delete_value(self.0);
        }
    }

    #[test]
    fn startup_command_quotes_paths_with_spaces() {
        let cmd = build_startup_command(Path::new(r"C:\Program Files\FineShot\fineshot.exe"));
        assert_eq!(cmd, r#""C:\Program Files\FineShot\fineshot.exe""#);
    }

    #[test]
    fn hkcu_run_roundtrip_on_isolated_test_value() {
        const TEST_VAL: &str = "FineShotUnitTestAutostartEntry";
        let _guard = CleanupGuard(TEST_VAL);

        let _ = delete_value(TEST_VAL);
        assert_eq!(query_value_exists(TEST_VAL), Ok(false));

        set_value(TEST_VAL, r#""C:\Temp\fineshot.exe""#).expect("write test run value");
        assert_eq!(query_value_exists(TEST_VAL), Ok(true));

        delete_value(TEST_VAL).expect("delete test run value");
        assert_eq!(query_value_exists(TEST_VAL), Ok(false));
    }
}
