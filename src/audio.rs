//! Микшер Windows через WASAPI.
//!
//! Работаем в отдельном потоке с COM в режиме MTA: раз в секунду читаем
//! список активных аудиосессий, их громкость и mute. Команды изменения
//! громкости приходят по каналу и применяются сразу, не дожидаясь опроса.
//!
//! Иконки приложений достаём через оболочку Windows и кэшируем по имени exe.
//! Поток максимально пассивный: при простое просыпается раз в секунду для
//! опроса микшера, команды из канала будят его сразу.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use image::RgbaImage;
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::shared::{AudioState, SessionInfo};

/// Как часто перечитываем микшер.
const POLL: Duration = Duration::from_secs(1);
/// Ограничение на разумный размер иконки, чтобы не выделять память зря.
const MAX_ICON: i32 = 512;

enum Cmd {
    SetVolume { app: String, volume: i32 },
    SetMute { app: String, muted: bool },
    PrimeIcon { app: String, path: String },
}

/// Ручка к фоновому потоку микшера.
#[derive(Clone)]
pub struct Audio {
    state: Arc<Mutex<Arc<AudioState>>>,
    tx: Sender<Cmd>,
}

impl Audio {
    /// Запускает поток. Ошибки COM не роняют приложение — они попадут
    /// в [`AudioState::error`], а интерфейс продолжит работать.
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(Arc::new(AudioState::default())));
        let (tx, rx) = channel();

        let thread_state = Arc::clone(&state);
        let spawned = std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || run(thread_state, rx));

        if let Err(e) = spawned {
            set_error(&state, format!("не удалось запустить поток аудио: {e}"));
        }

        Self { state, tx }
    }

    pub fn state(&self) -> Arc<Mutex<Arc<AudioState>>> {
        Arc::clone(&self.state)
    }

    /// Быстрый снимок состояния для рендера: копия `Arc`, без копирования сессий и иконок.
    pub fn snapshot(&self) -> Arc<AudioState> {
        self.state
            .lock()
            .map(|state| Arc::clone(&state))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }

    pub fn set_volume(&self, app: &str, volume: i32) {
        let _ = self.tx.send(Cmd::SetVolume {
            app: app.to_string(),
            volume,
        });
    }

    pub fn set_mute(&self, app: &str, muted: bool) {
        let _ = self.tx.send(Cmd::SetMute {
            app: app.to_string(),
            muted,
        });
    }

    /// Достаёт иконки для сохранённых приложений, даже если они не запущены.
    pub fn prime_icons(&self, items: &[(String, String)]) {
        for (app, path) in items {
            if !app.is_empty() && !path.is_empty() {
                let _ = self.tx.send(Cmd::PrimeIcon {
                    app: app.clone(),
                    path: path.clone(),
                });
            }
        }
    }
}

fn run(state: Arc<Mutex<Arc<AudioState>>>, rx: Receiver<Cmd>) {
    // COM освобождается только после выхода из цикла.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            set_error(&state, format!("COM MTA: {hr:?}"));
            return;
        }
    }

    let mixer = unsafe { Mixer::new() };
    let mixer = match mixer {
        Ok(mixer) => mixer,
        Err(e) => {
            set_error(&state, format!("микшер Windows: {e:#}"));
            unsafe { CoUninitialize() };
            return;
        }
    };

    let mut last_poll = Instant::now() - POLL;

    loop {
        let mut commands = Vec::new();
        let mut disconnected = false;
        match rx.recv_timeout(POLL.saturating_sub(last_poll.elapsed())) {
            Ok(cmd) => commands.push(cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => disconnected = true,
        }
        // Забираем всё, что накопилось: серия поворотов крутилки
        // схлопывается в одну установку громкости.
        loop {
            match rx.try_recv() {
                Ok(cmd) => commands.push(cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            drop(mixer);
            unsafe { CoUninitialize() };
            return;
        }

        // Команды выполняем раньше опроса: реакция на крутилку не должна
        // ждать следующего тика.
        if !commands.is_empty() {
            apply_batch(&mixer, &state, commands);
            last_poll = Instant::now();
        }

        if last_poll.elapsed() >= POLL {
            poll(&mixer, &state);
            last_poll = Instant::now();
        }
    }
}

/// Выполняет пачку команд, пропуская установку громкости, если дальше в пачке
/// есть более свежая для того же приложения.
fn apply_batch(mixer: &Mixer, state: &Arc<Mutex<Arc<AudioState>>>, commands: Vec<Cmd>) {
    let superseded: Vec<bool> = commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| match cmd {
            Cmd::SetVolume { app, .. } => commands[i + 1..]
                .iter()
                .any(|later| matches!(later, Cmd::SetVolume { app: other, .. } if other == app)),
            _ => false,
        })
        .collect();
    for (cmd, skip) in commands.into_iter().zip(superseded) {
        if !skip {
            apply(mixer, state, cmd);
        }
    }
}

fn apply(mixer: &Mixer, state: &Arc<Mutex<Arc<AudioState>>>, cmd: Cmd) {
    match cmd {
        Cmd::SetVolume { app, volume } => {
            let volume = volume.clamp(0, 100);
            match unsafe { mixer.set_volume(&app, volume) } {
                Ok(()) => {
                    // Сразу обновляем кэш, иначе до следующего опроса (до 1 с)
                    // плитка откатится к старому значению.
                    if let Ok(mut state) = state.lock() {
                        let state = Arc::make_mut(&mut state);
                        if let Some(session) = state.sessions.iter_mut().find(|s| s.app == app) {
                            session.volume = volume;
                        }
                    }
                }
                Err(e) => set_error(state, format!("громкость {app}: {e:#}")),
            }
        }
        Cmd::SetMute { app, muted } => match unsafe { mixer.set_mute(&app, muted) } {
            Ok(()) => {
                if let Ok(mut state) = state.lock() {
                    let state = Arc::make_mut(&mut state);
                    if let Some(session) = state.sessions.iter_mut().find(|s| s.app == app) {
                        session.muted = muted;
                    }
                }
            }
            Err(e) => set_error(state, format!("mute {app}: {e:#}")),
        },
        Cmd::PrimeIcon { app, path } => {
            let already = state
                .lock()
                .map(|s| s.icons.contains_key(&app))
                .unwrap_or(false);
            if already {
                return;
            }
            if let Some(icon) = load_icon(&path) {
                if let Ok(mut state) = state.lock() {
                    Arc::make_mut(&mut state).icons.insert(app, Arc::new(icon));
                }
            }
        }
    }
}

fn poll(mixer: &Mixer, state: &Arc<Mutex<Arc<AudioState>>>) {
    let found = match unsafe { mixer.collect() } {
        Ok(found) => found,
        Err(e) => {
            set_error(state, format!("{e:#}"));
            return;
        }
    };

    // Иконки извлекаем вне блокировки состояния: это самые дорогие вызовы.
    let known: Vec<String> = state
        .lock()
        .map(|s| s.icons.keys().cloned().collect())
        .unwrap_or_default();

    let mut new_icons = Vec::new();
    for session in &found {
        if session.app.is_empty() || session.exe_path.is_empty() {
            continue;
        }
        if known.iter().any(|app| app == &session.app) {
            continue;
        }
        if new_icons.iter().any(|(app, _): &(String, String)| app == &session.app) {
            continue;
        }
        new_icons.push((session.app.clone(), session.exe_path.clone()));
    }

    let loaded: Vec<(String, Arc<RgbaImage>)> = new_icons
        .into_iter()
        .filter_map(|(app, path)| load_icon(&path).map(|icon| (app, Arc::new(icon))))
        .collect();

    let sessions: Vec<SessionInfo> = found
        .into_iter()
        .map(|s| SessionInfo {
            app: s.app,
            name: s.name,
            exe_path: s.exe_path,
            volume: s.volume,
            muted: s.muted,
        })
        .collect();

    if let Ok(mut state) = state.lock() {
        let state = Arc::make_mut(&mut state);
        for (app, icon) in loaded {
            state.icons.entry(app).or_insert(icon);
        }
        state.sessions = sessions;
        state.error = None;
        state.ready = true;
    }
}

fn set_error(state: &Arc<Mutex<Arc<AudioState>>>, message: String) {
    eprintln!("[audio] {message}");
    if let Ok(mut state) = state.lock() {
        let state = Arc::make_mut(&mut state);
        state.error = Some(message);
        state.ready = false;
    }
}

struct Mixer {
    enumerator: IMMDeviceEnumerator,
    /// Регуляторы громкости из последнего перечисления сессий, по ключу приложения.
    controls: RefCell<HashMap<String, Vec<ISimpleAudioVolume>>>,
}

struct Found {
    app: String,
    name: String,
    exe_path: String,
    volume: i32,
    muted: bool,
    control: ISimpleAudioVolume,
}

impl Mixer {
    unsafe fn new() -> Result<Self> {
        let enumerator =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| anyhow!("CoCreateInstance: {e}"))?;
        Ok(Self { enumerator, controls: RefCell::new(HashMap::new()) })
    }

    unsafe fn collect(&self) -> Result<Vec<Found>> {
        let device = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let list = manager.GetSessionEnumerator()?;
        let count = list.GetCount()?;

        let mut found = Vec::new();
        for i in 0..count {
            let Ok(control) = list.GetSession(i as i32) else {
                continue;
            };
            if control
                .GetState()
                .map(|state| state == AudioSessionStateExpired)
                .unwrap_or(true)
            {
                continue;
            }

            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // Метод возвращает S_OK только для сессии системных звуков,
            // для остальных — S_FALSE (тоже «успех»), поэтому сравниваем HRESULT.
            if control2.IsSystemSoundsSession() == S_OK {
                continue;
            }

            let pid = control2.GetProcessId().unwrap_or(0);
            if pid == 0 || pid == std::process::id() {
                continue;
            }

            let exe_path = process_path(pid).unwrap_or_default();
            let (app, name) = app_key(&exe_path, pid);

            let Ok(volume_control) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            let volume = (volume_control.GetMasterVolume().unwrap_or(0.0) * 100.0)
                .round()
                .clamp(0.0, 100.0) as i32;
            let muted = volume_control
                .GetMute()
                .map(|value| value.as_bool())
                .unwrap_or(false);

            found.push(Found {
                app,
                name,
                exe_path,
                volume,
                muted,
                control: volume_control,
            });
        }

        let mut controls: HashMap<String, Vec<ISimpleAudioVolume>> = HashMap::new();
        for session in &found {
            controls.entry(session.app.clone()).or_default().push(session.control.clone());
        }
        *self.controls.borrow_mut() = controls;

        Ok(found)
    }

    unsafe fn set_volume(&self, app: &str, volume: i32) -> Result<()> {
        let level = volume as f32 / 100.0;
        self.for_each_control(app, |control| {
            control
                .SetMasterVolume(level, std::ptr::null())
                .map_err(|e| anyhow!("SetMasterVolume: {e}"))
        })
    }

    unsafe fn set_mute(&self, app: &str, muted: bool) -> Result<()> {
        self.for_each_control(app, |control| {
            control
                .SetMute(muted, std::ptr::null())
                .map_err(|e| anyhow!("SetMute: {e}"))
        })
    }

    /// Применяет действие к регуляторам приложения из кэша. Если кэш пуст или
    /// регулятор устарел, перечисляет сессии заново.
    unsafe fn for_each_control(
        &self,
        app: &str,
        action: impl Fn(&ISimpleAudioVolume) -> Result<()>,
    ) -> Result<()> {
        let cached = self.controls.borrow().get(app).cloned().unwrap_or_default();
        if !cached.is_empty() && cached.iter().all(|control| action(control).is_ok()) {
            return Ok(());
        }
        for session in self.collect()? {
            if session.app == app {
                action(&session.control)?;
            }
        }
        Ok(())
    }
}

/// Полный путь к исполняемому файлу процесса.
unsafe fn process_path(pid: u32) -> Option<String> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    let result = QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        PWSTR(buffer.as_mut_ptr()),
        &mut len,
    );
    let _ = CloseHandle(handle);

    result.ok()?;
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

fn app_key(path: &str, pid: u32) -> (String, String) {
    if path.is_empty() {
        return (format!("pid-{pid}"), format!("процесс {pid}"));
    }
    let file = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let app = file.to_lowercase();
    let name = Path::new(file)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(file)
        .to_string();
    (app, name)
}

// ---------------------------------------------------------------------------
// Иконки приложений
// ---------------------------------------------------------------------------

fn load_icon(path: &str) -> Option<RgbaImage> {
    if path.is_empty() {
        return None;
    }

    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut info = SHFILEINFOW::default();
        let result = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if result == 0 || info.hIcon.is_invalid() {
            return None;
        }

        let image = icon_to_rgba(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        image
    }
}

unsafe fn icon_to_rgba(icon: HICON) -> Option<RgbaImage> {
    let mut info = ICONINFO::default();
    if GetIconInfo(icon, &mut info).is_err() {
        return None;
    }

    let image = color_bitmap_to_rgba(info.hbmColor, info.hbmMask);

    if !info.hbmColor.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
    }
    if !info.hbmMask.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
    }

    image
}

unsafe fn color_bitmap_to_rgba(color: HBITMAP, mask: HBITMAP) -> Option<RgbaImage> {
    if color.is_invalid() {
        return None;
    }

    let mut bitmap = BITMAP::default();
    let got = GetObjectW(
        HGDIOBJ(color.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bitmap as *mut _ as *mut _),
    );
    if got == 0 {
        return None;
    }

    let width = bitmap.bmWidth;
    let height = bitmap.bmHeight;
    if width <= 0 || height <= 0 || width > MAX_ICON || height > MAX_ICON {
        return None;
    }

    let hdc = GetDC(None);
    if hdc.is_invalid() {
        return None;
    }

    let mut header = bitmap_info(width, -height);
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let lines = GetDIBits(
        hdc,
        color,
        0,
        height as u32,
        Some(pixels.as_mut_ptr() as *mut _),
        &mut header,
        DIB_RGB_COLORS,
    );
    if lines == 0 {
        ReleaseDC(None, hdc);
        return None;
    }

    // GDI отдаёт BGRA; заодно проверяем, есть ли вообще альфа-канал.
    let mut has_alpha = false;
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        if pixel[3] != 0 {
            has_alpha = true;
        }
    }

    // Старые иконки без альфы используют AND-маску: белое — прозрачное.
    if !has_alpha && !mask.is_invalid() {
        let mut mask_header = bitmap_info(width, -height);
        let mut mask_pixels = vec![0u8; (width * height * 4) as usize];
        let mask_lines = GetDIBits(
            hdc,
            mask,
            0,
            height as u32,
            Some(mask_pixels.as_mut_ptr() as *mut _),
            &mut mask_header,
            DIB_RGB_COLORS,
        );
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            pixel[3] = if mask_lines != 0 && mask_pixels[index * 4] > 127 {
                0
            } else {
                255
            };
        }
    }

    ReleaseDC(None, hdc);
    RgbaImage::from_raw(width as u32, height as u32, pixels)
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    info
}
