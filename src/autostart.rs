//! Автозапуск через `HKCU\...\Run` (текущий пользователь).

use std::path::PathBuf;

use windows::core::{w, PCWSTR, HSTRING};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY_CURRENT_USER,
    KEY_READ, KEY_SET_VALUE, REG_SZ,
};

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("AkpDeck");

/// Включён ли автозапуск в реестре.
pub fn is_enabled() -> bool {
    read_command().is_some()
}

/// Включает или выключает автозапуск текущим exe.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("путь к exe: {e}"))?;
        let quoted = format!("\"{}\"", exe.display());
        write_command(&quoted)
    } else {
        delete_command()
    }
}

/// Синхронизирует реестр с желаемым состоянием из конфига.
pub fn apply(want: bool) -> Result<(), String> {
    // Убрать старое имя ключа автозапуска, если было.
    let _ = delete_named(w!("AjazzDeck"));

    if want == is_enabled() {
        if want {
            if let Some(current) = read_command() {
                if let Ok(exe) = std::env::current_exe() {
                    let expected = format!("\"{}\"", exe.display());
                    if normalize(&current) != normalize(&expected) {
                        return write_command(&expected);
                    }
                }
            }
        }
        return Ok(());
    }
    set_enabled(want)
}

fn delete_named(name: PCWSTR) -> Result<(), String> {
    unsafe {
        let mut key = Default::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, Some(0), KEY_SET_VALUE, &mut key)
            != ERROR_SUCCESS
        {
            return Ok(());
        }
        let result = RegDeleteValueW(key, name);
        let _ = RegCloseKey(key);
        if result != ERROR_SUCCESS && result != ERROR_FILE_NOT_FOUND {
            return Err(format!("удаление автозапуска: {result:?}"));
        }
        Ok(())
    }
}

fn normalize(cmd: &str) -> String {
    cmd.trim().trim_matches('"').replace('/', "\\").to_ascii_lowercase()
}

fn read_command() -> Option<String> {
    unsafe {
        let mut key = Default::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, Some(0), KEY_READ, &mut key) != ERROR_SUCCESS
        {
            return None;
        }
        let mut kind = REG_SZ;
        let mut size = 0u32;
        let query = RegQueryValueExW(key, VALUE_NAME, None, Some(&mut kind), None, Some(&mut size));
        if query != ERROR_SUCCESS || size < 2 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let ok = RegQueryValueExW(
            key,
            VALUE_NAME,
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if ok != ERROR_SUCCESS || kind != REG_SZ {
            return None;
        }
        let u16s: Vec<u16> = buf[..size as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let s = String::from_utf16_lossy(&u16s);
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

fn write_command(command: &str) -> Result<(), String> {
    unsafe {
        let mut key = Default::default();
        let open = RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, Some(0), KEY_SET_VALUE, &mut key);
        if open != ERROR_SUCCESS {
            return Err(format!("реестр Run: {open:?}"));
        }
        let wide = HSTRING::from(command);
        let bytes =
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, (wide.len() + 1) * 2);
        let result = RegSetValueExW(key, VALUE_NAME, Some(0), REG_SZ, Some(bytes));
        let _ = RegCloseKey(key);
        if result != ERROR_SUCCESS {
            return Err(format!("запись автозапуска: {result:?}"));
        }
        Ok(())
    }
}

fn delete_command() -> Result<(), String> {
    delete_named(VALUE_NAME)
}

#[allow(dead_code)]
pub fn exe_path() -> Option<PathBuf> {
    std::env::current_exe().ok()
}
