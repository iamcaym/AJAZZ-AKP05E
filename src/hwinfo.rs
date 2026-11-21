//! HWiNFO как тихий источник CPU temp через Shared Memory.
//!
//! Портативная копия: `%LOCALAPPDATA%\akp-deck\hwinfo`.
//! INI: SensorsOnly + SensorsSM + minimize — summary не показываем.
//! Иконка в трее — ограничение HWiNFO (штатно не убрать).
//!
//! Free Shared Memory отключается ~через 12 ч — тогда тихо перезапускаем
//! только наш процесс. Бинарь качается с SourceForge при первом запуске;
//! в репозиторий/релиз akp-deck не входит.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Актуальный portable на SourceForge (`Windows_Portable/hwi_*.zip`).
const HWINFO_ZIP_URL: &str =
    "https://downloads.sourceforge.net/project/hwinfo/Windows_Portable/hwi_834.zip";
/// Чуть раньше лимита free Shared Memory (12 ч).
const SM_REFRESH: Duration = Duration::from_secs(11 * 3600);

static ENSURED: AtomicBool = AtomicBool::new(false);
static WE_SPAWNED: AtomicBool = AtomicBool::new(false);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static ENSURE_LOCK: Mutex<()> = Mutex::new(());
static LAST_ENSURE: Mutex<Option<Instant>> = Mutex::new(None);
static STARTED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Неблокирующий запрос: установка/запуск в фоне (не тормозит UI/тик).
pub fn request_ensure() {
    if mapping_alive() && process_running() {
        ENSURED.store(true, Ordering::Relaxed);
        return;
    }
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("hwinfo-ensure".into())
        .spawn(|| {
            ensure_running();
            IN_FLIGHT.store(false, Ordering::SeqCst);
        })
        .ok();
}

/// Ставит portable HWiNFO (если нужно) и запускает в sensors-only.
pub fn ensure_running() {
    let Ok(_guard) = ENSURE_LOCK.lock() else {
        return;
    };
    maybe_restart_for_sm_limit();

    if mapping_alive() && process_running() {
        ENSURED.store(true, Ordering::Relaxed);
        return;
    }

    if let Ok(guard) = LAST_ENSURE.lock() {
        // Не ретраим слишком часто, но если mapping мёртв — не блокируемся
        // «навсегда» из‑за ENSURED.
        if guard.is_some_and(|t| t.elapsed() < Duration::from_secs(8)) {
            return;
        }
    }

    let Some(dir) = hwinfo_dir() else {
        return;
    };
    if let Err(error) = install_if_needed(&dir) {
        eprintln!("[monitor] HWiNFO: {error}");
        return;
    }
    // INI только когда процесс ещё не запущен — иначе HWiNFO перезапишет.
    if !process_running() {
        write_ini(&dir);
        if let Err(error) = spawn_hwinfo(&dir) {
            eprintln!("[monitor] HWiNFO запуск: {error}");
            if let Ok(mut guard) = LAST_ENSURE.lock() {
                *guard = Some(Instant::now());
            }
            return;
        }
        println!("[monitor] HWiNFO sensors-only в фоне");
        WE_SPAWNED.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = STARTED_AT.lock() {
            *guard = Some(Instant::now());
        }
        for _ in 0..40 {
            if mapping_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    } else if !mapping_alive() {
        // Процесс есть, Shared Memory нет (free-лимит / welcome) — рестарт.
        println!("[monitor] HWiNFO без Shared Memory — перезапуск");
        let _ = silent_command("taskkill.exe", &["/IM", "HWiNFO64.exe", "/F"]);
        std::thread::sleep(Duration::from_millis(500));
        write_ini(&dir);
        if let Err(error) = spawn_hwinfo(&dir) {
            eprintln!("[monitor] HWiNFO запуск: {error}");
        } else {
            WE_SPAWNED.store(true, Ordering::Relaxed);
            if let Ok(mut guard) = STARTED_AT.lock() {
                *guard = Some(Instant::now());
            }
            for _ in 0..40 {
                if mapping_alive() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    ENSURED.store(mapping_alive(), Ordering::Relaxed);
    if let Ok(mut guard) = LAST_ENSURE.lock() {
        *guard = Some(Instant::now());
    }
}

fn maybe_restart_for_sm_limit() {
    if !WE_SPAWNED.load(Ordering::Relaxed) {
        return;
    }
    let started = STARTED_AT.lock().ok().and_then(|g| *g);
    let Some(started) = started else {
        return;
    };
    if started.elapsed() < SM_REFRESH {
        return;
    }
    if mapping_alive() {
        return;
    }
    println!("[monitor] HWiNFO: обновляю Shared Memory (лимит free ~12 ч)");
    kill_our_hwinfo();
    if let Ok(mut guard) = STARTED_AT.lock() {
        *guard = None;
    }
    ENSURED.store(false, Ordering::Relaxed);
    WE_SPAWNED.store(false, Ordering::Relaxed);
}

fn hwinfo_dir() -> Option<PathBuf> {
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    Some(base.join("akp-deck").join("hwinfo"))
}

fn exe_path(dir: &Path) -> PathBuf {
    dir.join("HWiNFO64.exe")
}

fn install_if_needed(dir: &Path) -> Result<(), String> {
    let exe = exe_path(dir);
    if exe.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("создать {dir:?}: {e}"))?;
    let zip = dir.join("hwinfo-portable.zip");
    println!("[monitor] скачиваю HWiNFO portable…");
    let status = silent_command(
        "curl.exe",
        &[
            "-fsSL",
            "-L",
            "--retry",
            "3",
            HWINFO_ZIP_URL,
            "-o",
            zip.to_str().ok_or("путь zip")?,
        ],
    )?;
    if !status.success() {
        return Err(format!("curl завершился с кодом {status}"));
    }
    let dir_str = dir.to_str().ok_or("путь hwinfo")?;
    let zip_str = zip.to_str().ok_or("путь zip")?;
    let expand = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        zip_str.replace('\'', "''"),
        dir_str.replace('\'', "''")
    );
    let status = silent_command(
        "powershell.exe",
        &["-NoProfile", "-WindowStyle", "Hidden", "-Command", &expand],
    )?;
    if !status.success() {
        return Err("не удалось распаковать HWiNFO".into());
    }
    let _ = std::fs::remove_file(&zip);
    if !exe.is_file() {
        if let Some(found) = find_exe_under(dir, 3) {
            if found != exe {
                let _ = std::fs::copy(&found, &exe);
            }
        }
    }
    if !exe.is_file() {
        return Err("после распаковки нет HWiNFO64.exe".into());
    }
    Ok(())
}

fn find_exe_under(dir: &Path, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let candidate = dir.join("HWiNFO64.exe");
    if candidate.is_file() {
        return Some(candidate);
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_exe_under(&path, depth - 1) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == "HWiNFO64.exe") {
            return Some(path);
        }
    }
    None
}

fn write_ini(dir: &Path) {
    let ini = dir.join("HWiNFO64.INI");
    let body = r#"[Settings]
SensorsOnly=1
SensorsSM=1
MinimalizeSensors=1
MinimalizeSensorsClose=1
MinimalizeMainWnd=1
OpenSystemSummary=0
ShowWelcomeAndProgress=0
AutoUpdate=0
AutoUpdateBetaDisable=1
PresentMon=0
Lang=en
"#;
    let _ = std::fs::write(ini, body);
}

fn spawn_hwinfo(dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let exe = exe_path(dir);
    // HWiNFO в манифесте requireAdministrator. Если akp-deck без elevation
    // (asInvoker) — CreateProcess даёт 740, тогда ShellExecute(runas).
    match Command::new(&exe)
        .current_dir(dir)
        .creation_flags(0x0000_0200) // CREATE_NEW_PROCESS_GROUP
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(error) if error.raw_os_error() == Some(740) => shell_execute_runas(&exe, dir),
        Err(error) => {
            // На всякий случай пробуем runas и при других отказах CreateProcess.
            match shell_execute_runas(&exe, dir) {
                Ok(()) => Ok(()),
                Err(runas_err) => Err(format!("spawn: {error}; runas: {runas_err}")),
            }
        }
    }
}

fn shell_execute_runas(exe: &Path, dir: &Path) -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe_h = HSTRING::from(exe.as_os_str());
    let dir_h = HSTRING::from(dir.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: windows::core::w!("runas"),
        lpFile: PCWSTR(exe_h.as_ptr()),
        lpDirectory: PCWSTR(dir_h.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| format!("ShellExecuteEx: {e}"))?;
        if !info.hProcess.is_invalid() {
            let _ = CloseHandle(info.hProcess);
        }
    }
    Ok(())
}

fn process_running() -> bool {
    silent_output(
        "tasklist.exe",
        &["/FI", "IMAGENAME eq HWiNFO64.exe", "/NH"],
    )
    .ok()
    .and_then(|out| String::from_utf8(out).ok())
    .is_some_and(|text| text.to_ascii_lowercase().contains("hwinfo64.exe"))
}

fn mapping_alive() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Memory::{OpenFileMappingW, FILE_MAP_READ};
    unsafe {
        match OpenFileMappingW(
            FILE_MAP_READ.0,
            false,
            windows::core::w!("Global\\HWiNFO_SENS_SM2"),
        ) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

fn kill_our_hwinfo() {
    // Только если мы сами поднимали процесс — иначе чужой HWiNFO не трогаем.
    if !WE_SPAWNED.load(Ordering::Relaxed) {
        return;
    }
    let _ = silent_command("taskkill.exe", &["/IM", "HWiNFO64.exe", "/F"]);
}

fn silent_command(program: &str, args: &[&str]) -> Result<std::process::ExitStatus, String> {
    use std::os::windows::process::CommandExt;
    Command::new(program)
        .args(args)
        .creation_flags(0x0800_0000)
        .status()
        .map_err(|e| format!("{program}: {e}"))
}

fn silent_output(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    use std::os::windows::process::CommandExt;
    Command::new(program)
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
        .map(|out| out.stdout)
        .map_err(|e| format!("{program}: {e}"))
}
