#![windows_subsystem = "windows"]

//! AKP Deck — клиент для панели AKP05E: мониторинг ПК + окно конфигурации.
//!
//! Устройство обслуживается асинхронным циклом на tokio-рантайме (главный
//! поток), окно конфигурации egui живёт в отдельном потоке. Общаются они
//! через [`Shared`] (Mutex). Закрытие окна прячет его в трей, приложение
//! завершается пунктом «Закрыть» в меню значка.
//!
//! Ключевая особенность устройства: оно не отдаёт события ввода, пока
//! разбирает загруженную картинку. Поэтому плитки заливаются по одной
//! с паузой `device.upload_gap_ms`, и только те, что реально изменились.

mod audio;
mod actions;
mod autostart;
mod config;
mod device;
mod monitor;
mod render;
mod shared;
mod tray;
mod ui;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use image::{DynamicImage, Rgb, RgbImage};
use mirajazz::state::DeviceStateReader;
use mirajazz::types::{DeviceInput, ImageFormat};
use tokio::sync::mpsc;
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use crate::config::{Config, KeyCfg};
use crate::device::DeckEvent;
use crate::monitor::{Monitor, Snapshot};
use crate::render::{Renderer, TilePalette};
use crate::shared::{AudioState, Shared};

/// Минимальный интервал опроса метрик.
const MIN_METRICS_MS: u64 = 250;
/// Минимальная пауза между заливками.
const MIN_UPLOAD_GAP_MS: u64 = 20;
/// Период keep-alive.
const KEEPALIVE: Duration = Duration::from_secs(10);
/// Как часто проверяем автоматически сохраняемый конфиг для быстрого обновления устройства.
const CFG_CHECK: Duration = Duration::from_millis(50);
/// Как часто проверяем время изменения config.toml (ручная правка файла).
const MTIME_CHECK: Duration = Duration::from_secs(1);
/// Небольшой интервал между кадрами при полной перерисовке.
const FAST_GAP_MS: u64 = 40;
/// Сколько ждём после последнего события крутилки перед финальной заливкой.
const ZONE_SETTLE: Duration = Duration::from_millis(15);
/// Частота пробуждения цикла, пока крутилка ждёт отрисовки.
const ZONE_POLL: Duration = Duration::from_millis(20);
/// Первая пауза перед повторным подключением.
const RECONNECT_MIN: Duration = Duration::from_secs(2);
/// Максимальная пауза перед повторным подключением, пока устройства нет.
const RECONNECT_MAX: Duration = Duration::from_secs(10);
/// После последнего события крутилки плановые плитки не шлём это время.
const INTERACTIVE_HOLD: Duration = Duration::from_millis(1500);
/// Смещение ключа `last_render` для пустых зон, чтобы не пересекаться с номерами клавиш.
const BLANK_ZONE_KEY: u8 = 100;

fn main() -> Result<()> {
    // Права администратора — только через app.manifest (requireAdministrator).
    // Программный ShellExecute(runas) поверх манифеста давал двойной UAC /
    // «окно не открывается» — больше не используем.
    let Some(_instance_mutex) = acquire_single_instance()? else {
        println!("[app] уже запущено: активирую существующее окно");
        activate_existing_window();
        return Ok(());
    };

    println!("akp-deck {}", env!("CARGO_PKG_VERSION"));
    println!("[config] {}", config::config_path().display());

    let shared = Arc::new(Mutex::new(Shared::default()));
    let (initial_config, config_error) = config::load();
    if let Err(error) = autostart::apply(initial_config.app.autostart) {
        eprintln!("[autostart] {error}");
    }
    {
        let mut state = shared_lock(&shared);
        state.active_scene = initial_config.active_scene;
        state.config = initial_config;
        if let Some(message) = &config_error {
            state.push_log(message.clone());
        }
        state.config_error = config_error;
    }

    // Микшер Windows живёт отдельным потоком всё время работы приложения.
    let audio = audio::Audio::start();
    shared_lock(&shared).audio = audio.state();

    let stop = Arc::new(AtomicBool::new(false));

    // Поток устройства (tokio). Окно egui должно жить на главном потоке.
    let device_shared = Arc::clone(&shared);
    let device_stop = Arc::clone(&stop);
    let device_audio = audio.clone();
    let device_thread = std::thread::Builder::new()
        .name("device".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build();

            match runtime {
                Ok(runtime) => {
                    runtime.block_on(device_main(&device_shared, &device_stop, &device_audio))
                }
                Err(e) => {
                    eprintln!("[runtime] {e}");
                    device_stop.store(true, Ordering::SeqCst);
                }
            }
        })
        .expect("не удалось создать поток устройства");

    // Значок в трее. Без него закрытие окна завершает приложение, как раньше.
    let icon = app_icon();
    match tray::start(Arc::clone(&shared), icon.rgba.clone(), icon.width) {
        Ok(()) => shared_lock(&shared).hide_to_tray = true,
        Err(e) => eprintln!("[tray] {e:#}"),
    }

    // Окно конфигурации на главном потоке.
    let ui_shared = Arc::clone(&shared);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("AKP Deck — конфигурация")
            .with_icon(icon)
            .with_inner_size([1240.0, 860.0])
            .with_min_inner_size([1000.0, 680.0]),
        ..Default::default()
    };

    let ui_result = eframe::run_native(
        "akp-deck",
        options,
        Box::new(move |cc| {
            ui::setup(&cc.egui_ctx);
            shared_lock(&ui_shared).egui_ctx = Some(cc.egui_ctx.clone());
            Ok(Box::new(ui::ConfigApp::new(Arc::clone(&ui_shared))))
        }),
    );

    match ui_result {
        Ok(()) => println!("[ui] окно закрыто, завершение работы"),
        Err(e) => eprintln!("[ui] окно не открылось: {e}"),
    }

    tray::stop();
    stop.store(true, Ordering::SeqCst);
    let _ = device_thread.join();

    println!("[app] завершено");
    Ok(())
}

/// Original geometric app mark (no third party artwork).
fn app_icon() -> eframe::egui::IconData {
    let size = 64usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - 31.5;
            let dy = y as f32 - 31.5;
            let i = (y * size + x) * 4;
            if dx * dx + dy * dy < 31.0 * 31.0 {
                rgba[i..i + 4].copy_from_slice(&[30, 105, 190, 255]);
            }
            let inside_a = (y > 15 && y < 50 && (x as f32 - 31.5).abs() < (y as f32 - 15.0) * 0.48)
                || (y > 31 && y < 38 && x > 20 && x < 44);
            if inside_a { rgba[i..i + 4].copy_from_slice(&[245, 249, 255, 255]); }
        }
    }
    eframe::egui::IconData { rgba, width: size as u32, height: size as u32 }
}

/// Один экземпляр (v3 — после отказа от dual-mutex elevated/user).
fn acquire_single_instance() -> Result<Option<HANDLE>> {
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\AkpDeck.Singleton.v3"))? };
    if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex)? };
        Ok(None)
    } else {
        Ok(Some(mutex))
    }
}

/// Показывает окно уже запущенного экземпляра, даже если оно спрятано в трей.
/// Если трея нет, ждёт создания окна и выводит его вперёд.
fn activate_existing_window() {
    for _ in 0..20 {
        if tray::show_existing() {
            return;
        }
        let window = unsafe { FindWindowW(None, w!("AKP Deck — конфигурация")) };
        if let Ok(window) = window {
            if !window.is_invalid() {
                unsafe {
                    let _ = ShowWindow(window, SW_RESTORE);
                    let _ = SetForegroundWindow(window);
                }
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("[app] существующий процесс найден, но его окно пока недоступно");
}

async fn device_main(
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    audio: &audio::Audio,
) {
    let mut reconnect_delay: Option<Duration> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }

        let cfg = shared_lock(shared).config.clone();

        let started = Instant::now();
        match run(cfg, shared, stop, audio).await {
            Ok(message) => {
                println!("[app] {message}");
                status(shared, message);
            }
            Err(e) => {
                eprintln!("[app] ошибка: {e:#}");
                status(shared, format!("ошибка: {e:#}"));
            }
        }

        // Пока устройства нет, пауза растёт до RECONNECT_MAX; после
        // рабочей сессии переподключаемся быстро.
        let delay = match reconnect_delay {
            Some(previous) if started.elapsed() < RECONNECT_MAX => (previous * 2).min(RECONNECT_MAX),
            _ => RECONNECT_MIN,
        };
        reconnect_delay = Some(delay);

        // Ждём переподключения, с проверкой флага остановки.
        let wake_at = Instant::now() + delay;
        while Instant::now() < wake_at {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn run(
    cfg: Config,
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    audio: &audio::Audio,
) -> Result<String> {
    let deck = device::connect(cfg.device.vid, cfg.device.pid).await?;
    let connected = format!("подключено, серийник {}", deck.serial);
    println!("[deck] {connected}");
    status(shared, connected.clone());
    log(shared, connected.clone());

    prime_zone_icons(audio, &cfg);

    deck.dev
        .set_brightness(cfg.device.brightness)
        .await
        .map_err(|e| anyhow!("установка яркости: {e}"))?;

    // Стираем boot-логотип Ajazz (белый фон), иначе наши плитки рисуются поверх.
    // На protocol v3 clear_all сам шлёт STP — экран гаснет до первой заливки.
    deck.dev
        .clear_all_button_images()
        .await
        .map_err(|e| anyhow!("очистка экрана: {e}"))?;
    println!("[deck] экран очищен");

    let renderer = Renderer::new()?;
    let mut monitor = Monitor::new(cfg.monitor.gpu_index)?;

    let reader = deck.dev.get_reader(|_, _| Ok(DeviceInput::NoData));
    let (tx, rx) = mpsc::unbounded_channel();
    let input = tokio::spawn(input_task(reader, tx));

    println!(
        "[app] общий интервал {} мс, пауза заливки {} мс",
        cfg.monitor.interval_ms.max(MIN_METRICS_MS),
        cfg.device.upload_gap_ms.max(MIN_UPLOAD_GAP_MS)
    );

    let result = app_loop(deck, cfg, renderer, &mut monitor, rx, shared, stop, audio).await;

    input.abort();
    result
}

#[allow(clippy::too_many_arguments)]
async fn app_loop(
    deck: device::Deck,
    mut cfg: Config,
    renderer: Renderer,
    monitor: &mut Monitor,
    mut rx: mpsc::UnboundedReceiver<InputMsg>,
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    audio: &audio::Audio,
) -> Result<String> {
    let mut page_index = shared_lock(shared)
        .active_page
        .min(cfg.active_pages().len().saturating_sub(1));
    let mut volumes = [50i32; device::ENCODER_COUNT];
    let mut muted = [None; device::ENCODER_COUNT];
    let mut queue = UploadQueue::default();
    // После смены страницы, сцены или конфига очередь сливается с короткой паузой.
    let mut burst = true;
    let mut sent: HashMap<u8, u64> = HashMap::new();
    let mut last_render: HashMap<u8, Instant> = HashMap::new();
    let mut last_zones: Option<Instant> = None;

    sync_audio(&cfg, page_index, audio, &mut volumes, &mut muted, shared);
    monitor.set_needs(monitor_needs(&cfg));

    shared_lock(shared).volumes = volumes;
    shared_lock(shared).muted = std::array::from_fn(|i| muted[i].is_some());

    let mut snapshot = monitor.poll();
    shared_lock(shared).snapshot = Some(snapshot.clone());

    let audio_state = audio.snapshot();
    let now = Instant::now();
    redraw_all(
        &renderer,
        &cfg,
        page_index,
        &snapshot,
        &volumes,
        &muted,
        &audio_state,
        &mut queue,
        &mut sent,
        &mut last_render,
        true,
        now,
    )
    .await?;

    let mut next_metrics = now + effective_interval(&cfg, page_index);
    let mut next_keepalive = now + KEEPALIVE;
    let mut next_upload = now;
    let mut next_cfg_check = now + CFG_CHECK;
    let mut next_audio = now + Duration::from_secs(1);
    let mut next_dynamic = now;
    let mut last_upload_at: Option<Instant> = None;
    let mut last_flush_at: Option<Instant> = None;
    let mut cfg_mtime = config::mtime();
    let mut next_mtime_check = now + MTIME_CHECK;
    let mut applied_config_revision = shared_lock(shared).config_revision;

    // Интерактивное состояние крутилок: серия событий сливается в одну
    // плитку, которая уходит сразу, без очереди.
    let mut zone_pending: Option<ZonePending> = None;
    let mut last_zone_event: Option<Instant> = None;
    let mut last_zone_upload: Option<Instant> = None;
    let mut interactive_until: Option<Instant> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            // При выходе из приложения экран не должен держать последний кадр.
            let _ = deck.dev.clear_all_button_images().await;
            let _ = deck.dev.flush().await;
            let _ = deck.dev.sleep().await;
            return Ok("остановлено".to_string());
        }

        let now = Instant::now();
        let mut wake = next_metrics.min(next_keepalive).min(next_cfg_check).min(next_audio).min(next_dynamic);
        if !queue.is_empty() {
            wake = wake.min(next_upload);
        }
        // Пока крутилка ждёт отрисовки, просыпаемся часто, чтобы слить
        // серию поворотов в одну плитку и отправить её немедленно.
        if zone_pending.is_some() {
            wake = wake.min(now + ZONE_POLL);
        }
        let sleep_for = wake.saturating_duration_since(now);

        tokio::select! {
            biased;
            maybe = rx.recv() => {
                match maybe {
                    Some(InputMsg::Event(event)) => {
                        let page_count = cfg.active_pages().len().max(1);
                        let requested_page = match &event {
                            DeckEvent::Swipe { left } => {
                                let action = cfg.active_pages().get(page_index).map(|page| {
                                    if *left { page.swipe_left.clone() } else { page.swipe_right.clone() }
                                });
                                if let Some(action) = action.as_ref() {
                                    match actions::run_tile_action(action) {
                                        Ok(actions::ActionEffect::NextPage) => {
                                            Some((page_index + 1).min(page_count.saturating_sub(1)))
                                        }
                                        Ok(actions::ActionEffect::PrevPage) => {
                                            Some(page_index.saturating_sub(1))
                                        }
                                        Ok(actions::ActionEffect::None) => {
                                            log(shared, format!(
                                                "свайп {}: {}",
                                                if *left { "влево" } else { "вправо" },
                                                action.label()
                                            ));
                                            None
                                        }
                                        Err(error) => {
                                            let message = format!(
                                                "свайп {}: {error:#}",
                                                if *left { "влево" } else { "вправо" }
                                            );
                                            eprintln!("[action] {message}");
                                            log(shared, message);
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            }
                            DeckEvent::Key { key, down: true }
                                if cfg.active_pages().get(page_index).is_some_and(|page| {
                                    page.keys.iter().any(|item| {
                                        matches!(item, KeyCfg::NextPage { index, .. } if *index == *key)
                                    })
                                }) => Some((page_index + 1) % page_count),
                            DeckEvent::Key { key, down: true } => {
                                if let Some(action) = cfg
                                    .active_pages()
                                    .get(page_index)
                                    .and_then(|page| page.keys.iter().find(|item| item.index() == *key))
                                {
                                    let tile = match action {
                                        KeyCfg::Launch { path, .. } => {
                                            Some(crate::config::TileAction::Launch { path: path.clone() })
                                        }
                                        KeyCfg::Hotkey { key, ctrl, alt, shift, win, .. } => {
                                            Some(crate::config::TileAction::Hotkey {
                                                key: key.clone(),
                                                ctrl: *ctrl,
                                                alt: *alt,
                                                shift: *shift,
                                                win: *win,
                                            })
                                        }
                                        KeyCfg::Media { action, .. } => {
                                            Some(crate::config::TileAction::Media(*action))
                                        }
                                        KeyCfg::Metric { on_press, .. } if !on_press.is_no_action() => {
                                            Some(on_press.clone())
                                        }
                                        _ => None,
                                    };
                                    if let Some(tile) = tile {
                                        match actions::run_tile_action(&tile) {
                                            Ok(actions::ActionEffect::NextPage) => {
                                                Some((page_index + 1) % page_count)
                                            }
                                            Ok(actions::ActionEffect::PrevPage) => {
                                                Some(page_index.saturating_sub(1))
                                            }
                                            Ok(actions::ActionEffect::None) => {
                                                log(shared, format!("клавиша {key:02}: {}", tile.label()));
                                                None
                                            }
                                            Err(error) => {
                                                let message = format!("клавиша {key:02}: {error:#}");
                                                eprintln!("[action] {message}");
                                                log(shared, message);
                                                None
                                            }
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            DeckEvent::TouchTap { zone } => {
                                let action = cfg
                                    .active_pages()
                                    .get(page_index)
                                    .and_then(|page| page.zones.iter().find(|z| z.index == *zone))
                                    .map(|z| z.tap_action.clone())
                                    .unwrap_or_default();
                                match actions::run_tile_action(&action) {
                                    Ok(actions::ActionEffect::NextPage) => {
                                        Some((page_index + 1) % page_count)
                                    }
                                    Ok(actions::ActionEffect::PrevPage) => {
                                        Some(page_index.saturating_sub(1))
                                    }
                                    Ok(actions::ActionEffect::None) => {
                                        if !action.is_no_action() {
                                            log(shared, format!("сенсор {zone}: {}", action.label()));
                                        }
                                        None
                                    }
                                    Err(error) => {
                                        let message = format!("сенсор {zone}: {error:#}");
                                        eprintln!("[action] {message}");
                                        log(shared, message);
                                        None
                                    }
                                }
                            }
                            _ => None,
                        };
                        if let Some(next_page) = requested_page.filter(|next| *next != page_index) {
                            page_index = next_page;
                            next_metrics = Instant::now() + effective_interval(&cfg, page_index);
                            queue = UploadQueue::default();
                            burst = true;
                            next_upload = Instant::now();
                            sent.clear();
                            last_render.clear();
                            last_zones = None;
                            zone_pending = None;
                            shared_lock(shared).active_page = page_index;
                            sync_audio(&cfg, page_index, audio, &mut volumes, &mut muted, shared);
                            let audio_state = audio.snapshot();
                            redraw_all(
                                &renderer, &cfg, page_index, &snapshot, &volumes, &muted,
                                &audio_state, &mut queue, &mut sent, &mut last_render, true, now,
                            ).await?;
                            log(shared, format!("страница: {}", cfg.active_pages()[page_index].name));
                            continue;
                        }
                        if let Some(zone) = handle_event(event, &cfg, page_index, &mut volumes, &mut muted, audio, shared).await? {
                            zone_pending = Some(zone);
                            last_zone_event = Some(Instant::now());
                            interactive_until = Some(Instant::now() + INTERACTIVE_HOLD);
                        }
                    }
                    Some(InputMsg::Ack) => {
                        if let Some(flushed) = last_flush_at.take() {
                            let ms = flushed.elapsed().as_millis();
                            if ms >= 30 {
                                println!("[ack] картинка подтверждена через {ms} мс");
                            }
                        }
                    }
                    Some(InputMsg::Closed(reason)) => {
                        return Err(anyhow!("поток ввода остановился: {reason}"));
                    }
                    None => return Err(anyhow!("канал ввода закрыт")),
                }
            }
            _ = tokio::time::sleep(sleep_for) => {}
        }

        let now = Instant::now();
        let interactive = interactive_until.is_some_and(|t| now < t);

        // 1. Зона крутилки — высший приоритет. Отправляем не чаще
        //    частоты обновления экрана, но не позднее ZONE_SETTLE после
        //    последнего события, чтобы панель всегда догоняла руку.
        let zone_cool =
            last_zone_upload.is_none_or(|t| now.duration_since(t) >= refresh_gap(&cfg));
        let zone_settled =
            last_zone_event.is_none_or(|t| now.duration_since(t) >= ZONE_SETTLE);

        if zone_pending.is_some() && (zone_cool || zone_settled) {
            if let Some(zone) = zone_pending.take() {
                let audio_state = audio.snapshot();
                upload_zone(
                    &deck,
                    &renderer,
                    &cfg,
                    page_index,
                    &zone,
                    &snapshot,
                    &audio_state,
                    &mut queue,
                    &mut sent,
                )
                .await?;
                last_zone_upload = Some(Instant::now());
                last_upload_at = Some(Instant::now());
                last_flush_at = Some(Instant::now());
            }
        } else if !interactive {
            // 2. Плановые плитки — только когда крутилки не трогают.
            if !queue.is_empty() && now >= next_upload {
                if let Some((wire, jpeg)) = queue.pop() {
                    // Большая очередь (после «Применить») сливается быстрее.
                    let gap = if burst || queue.len() > 8 {
                        Duration::from_millis(FAST_GAP_MS)
                    } else {
                        Duration::from_millis(cfg.device.upload_gap_ms.max(MIN_UPLOAD_GAP_MS))
                    };

                    if let Some(last) = last_upload_at {
                        if let Some(wait) = gap.checked_sub(last.elapsed()) {
                            tokio::time::sleep(wait).await;
                        }
                    }

                    let started = Instant::now();
                    deck.dev.write_image(wire, &jpeg).await?;
                    deck.dev.flush().await?;
                    last_upload_at = Some(Instant::now());
                    last_flush_at = Some(Instant::now());
                    next_upload = Instant::now() + gap;
                    if queue.is_empty() {
                        burst = false;
                    }

                    let ms = started.elapsed().as_millis();
                    if ms >= 60 {
                        log(shared, format!("плитка {wire}: {ms} мс"));
                    }
                }
            }
        }

        if !interactive && now >= next_audio {
            next_audio = now + Duration::from_secs(1);
            if sync_audio(&cfg, page_index, audio, &mut volumes, &mut muted, shared) {
                let audio_state = audio.snapshot();
                redraw_all(
                    &renderer,
                    &cfg,
                    page_index,
                    &snapshot,
                    &volumes,
                    &muted,
                    &audio_state,
                    &mut queue,
                    &mut sent,
                    &mut last_render,
                    true,
                    now,
                )
                .await?;
                last_zones = Some(now);
            }
        }

        if !interactive && now >= next_dynamic {
            next_dynamic = now + Duration::from_secs(5);
            let audio_state = audio.snapshot();
            let sessions: Vec<(&str, &str, &str)> = audio_state
                .sessions
                .iter()
                .map(|session| (session.app.as_str(), session.name.as_str(), session.exe_path.as_str()))
                .collect();
            let mixers_changed = cfg.audio.merge_sessions(sessions.iter().copied());
            let dynamic_changed = cfg.resolve_dynamic_apps(sessions.iter().copied());
            if mixers_changed || dynamic_changed {
                {
                    let mut state = shared_lock(shared);
                    if mixers_changed {
                        state.config.audio.mixers = cfg.audio.mixers.clone();
                    }
                    if dynamic_changed {
                        state.config.scenes = cfg.scenes.clone();
                        state.push_log("динамический захват обновил микшеры".into());
                    }
                }
                if dynamic_changed {
                    sync_audio(&cfg, page_index, audio, &mut volumes, &mut muted, shared);
                    prime_zone_icons(audio, &cfg);
                    redraw_all(
                        &renderer,
                        &cfg,
                        page_index,
                        &snapshot,
                        &volumes,
                        &muted,
                        &audio_state,
                        &mut queue,
                        &mut sent,
                        &mut last_render,
                        true,
                        now,
                    )
                    .await?;
                    last_zones = Some(now);
                }
            }
        }

        if now >= next_metrics {
            snapshot = monitor.poll();
            next_metrics = Instant::now() + effective_interval(&cfg, page_index);

            shared_lock(shared).snapshot = Some(snapshot.clone());

            let zones_due = last_zones
                .map(|t| now.duration_since(t) >= Duration::from_millis(cfg.monitor.interval_ms))
                .unwrap_or(true);

            let audio_state = audio.snapshot();
            redraw_all(
                &renderer,
                &cfg,
                page_index,
                &snapshot,
                &volumes,
                &muted,
                &audio_state,
                &mut queue,
                &mut sent,
                &mut last_render,
                zones_due,
                now,
            )
            .await?;

            if zones_due {
                last_zones = Some(now);
            }
        }

        // Конфиг проверяем часто и независимо от интервала метрик,
        // чтобы «Применить» в окне срабатывало сразу.
        if now >= next_cfg_check {
            next_cfg_check = now + CFG_CHECK;

            // Ревизию читаем раньше mtime: окно сначала пишет файл, потом
            // поднимает ревизию, поэтому собственное сохранение не даст
            // повторной перезагрузки из файла.
            let ui_changed_config = shared_lock(shared).config_revision != applied_config_revision;
            let mtime = if ui_changed_config || now >= next_mtime_check {
                next_mtime_check = now + MTIME_CHECK;
                config::mtime()
            } else {
                cfg_mtime
            };
            let new_cfg = if ui_changed_config {
                let state = shared_lock(shared);
                applied_config_revision = state.config_revision;
                Some((state.config.clone(), state.active_page))
            } else if mtime != cfg_mtime {
                match config::reload() {
                    Ok(file_cfg) => {
                        let mut state = shared_lock(shared);
                        state.config_error = None;
                        Some((file_cfg, state.active_page))
                    }
                    Err(message) => {
                        let mut state = shared_lock(shared);
                        state.push_log(message.clone());
                        state.config_error = Some(message);
                        None
                    }
                }
            } else {
                None
            };
            cfg_mtime = mtime;
            if let Some((new_cfg, ui_page)) = new_cfg {
                if new_cfg.device.brightness != cfg.device.brightness {
                    deck.dev
                        .set_brightness(new_cfg.device.brightness)
                        .await
                        .map_err(|e| anyhow!("установка яркости: {e}"))?;
                }

                cfg = new_cfg;
                if monitor.set_needs(monitor_needs(&cfg)) {
                    snapshot = monitor.poll();
                    shared_lock(shared).snapshot = Some(snapshot.clone());
                }
                page_index = ui_page.min(cfg.active_pages().len().saturating_sub(1));
                next_metrics = Instant::now() + effective_interval(&cfg, page_index);
                prime_zone_icons(audio, &cfg);
                last_render.clear();
                last_zones = None;
                burst = true;
                next_upload = Instant::now();
                shared_lock(shared).config = cfg.clone();
                shared_lock(shared).active_page = page_index;
                shared_lock(shared).active_scene = cfg.active_scene;
                log(shared, "конфиг применён, плитки перерисовываются".to_string());
                println!("[config] применён, перерисовываю плитки");

                // Сразу перерисовываем всё по новому конфигу, не дожидаясь
                // следующего тика метрик.
                let audio_state = audio.snapshot();
                sync_audio(&cfg, page_index, audio, &mut volumes, &mut muted, shared);
                redraw_all(
                    &renderer,
                    &cfg,
                    page_index,
                    &snapshot,
                    &volumes,
                    &muted,
                    &audio_state,
                    &mut queue,
                    &mut sent,
                    &mut last_render,
                    true,
                    Instant::now(),
                )
                .await?;
            }
        }

        if now >= next_keepalive {
            if let Err(e) = deck.dev.keep_alive().await {
                eprintln!("[deck] keep-alive: {e}");
            }
            next_keepalive = Instant::now() + KEEPALIVE;
        }
    }
}

/// Минимальный интервал между перерисовками одной плитки по частоте экрана.
fn refresh_gap(cfg: &Config) -> Duration {
    Duration::from_millis(1000 / cfg.device.refresh_hz.clamp(1, 60) as u64)
}

/// Эффективный период опроса: минимум из общего интервала и интервалов
/// отдельных плиток.
fn effective_interval(cfg: &Config, page_index: usize) -> Duration {
    let global = cfg.monitor.interval_ms.max(MIN_METRICS_MS);
    let min_key = cfg
        .active_pages()
        .get(page_index)
        .into_iter()
        .flat_map(|page| page.keys.iter())
        .filter_map(|key| match key {
            KeyCfg::Metric {
                interval_ms: Some(ms),
                ..
            } => Some((*ms).max(MIN_METRICS_MS)),
            _ => None,
        })
        .min()
        .unwrap_or(global);

    Duration::from_millis(global.min(min_key))
}

/// Источники метрик, которые используются хоть на одной странице активной сцены.
fn monitor_needs(cfg: &Config) -> monitor::Needs {
    monitor::Needs::from_metrics(cfg.active_pages().iter().flat_map(|page| {
        page.keys
            .iter()
            .filter_map(|key| match key {
                KeyCfg::Metric { metric, .. } => Some(*metric),
                _ => None,
            })
            .chain(page.zones.iter().filter_map(|zone| zone.metric))
    }))
}

enum InputMsg {
    Event(DeckEvent),
    Ack,
    Closed(String),
}

async fn input_task(reader: Arc<DeviceStateReader>, tx: mpsc::UnboundedSender<InputMsg>) {
    loop {
        match reader
            .raw_read_data_with_timeout(1025, Duration::from_millis(300))
            .await
        {
            Ok(Some(buf)) => {
                // Короткий пакет подтверждения загрузки картинки: байт 9
                // равен нулю. Он нужен только для замера задержки flush.
                if buf.get(9) == Some(&0) {
                    if tx.send(InputMsg::Ack).is_err() {
                        return;
                    }
                } else if let Some(event) = device::parse(&buf) {
                    if tx.send(InputMsg::Event(event)).is_err() {
                        return;
                    }
                } else {
                    static LAST_UNKNOWN: std::sync::Mutex<Option<(u8, u8, Instant)>> =
                        std::sync::Mutex::new(None);
                    let tag = buf.get(9).copied().unwrap_or(0);
                    let state = buf.get(10).copied().unwrap_or(0);
                    let now = Instant::now();
                    let mut guard = LAST_UNKNOWN.lock().unwrap_or_else(|e| e.into_inner());
                    let repeat = guard.is_some_and(|(t, s, at)| t == tag && s == state && now.duration_since(at) < Duration::from_secs(2));
                    if !repeat {
                        *guard = Some((tag, state, now));
                        println!("[input] неизвестный пакет {}", device::describe_raw(&buf));
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(InputMsg::Closed(format!("{e}")));
                return;
            }
        }
    }
}

/// Обновление зоны крутилки, ожидающее заливки.
struct ZonePending {
    encoder: u8,
    volume: i32,
    muted: bool,
}

/// Обрабатывает событие ввода. Возвращает обновление зоны, если оно есть;
/// сама заливка происходит в главном цикле, чтобы серия поворотов слилась
/// в одну плитку.
async fn handle_event(
    event: DeckEvent,
    cfg: &Config,
    page_index: usize,
    volumes: &mut [i32; device::ENCODER_COUNT],
    muted: &mut [Option<i32>; device::ENCODER_COUNT],
    audio: &audio::Audio,
    shared: &Arc<Mutex<Shared>>,
) -> Result<Option<ZonePending>> {
    match event {
        DeckEvent::Key { key, down } => {
            let message = format!(
                "клавиша {key:02} {}",
                if down { "нажата" } else { "отпущена" }
            );
            println!("[key] {message}");
            log(shared, message);
            Ok(None)
        }
        DeckEvent::Press { encoder } => {
            let press_action = cfg
                .active_pages()
                .get(page_index)
                .and_then(|page| page.encoders.iter().find(|item| item.index == encoder))
                .map(|item| item.press_action.clone())
                .unwrap_or(crate::config::EncoderPressAction::MuteApplication);
            if press_action == crate::config::EncoderPressAction::NoAction {
                return Ok(None);
            }
            if let crate::config::EncoderPressAction::Media(action) = &press_action {
                match actions::send_media(*action) {
                    Ok(()) => log(shared, format!("крутилка {encoder}: {}", action.label())),
                    Err(error) => log(shared, format!("крутилка {encoder}: не удалось отправить команду мультимедиа: {error:#}")),
                }
                return Ok(None);
            }
            if let crate::config::EncoderPressAction::Hotkey { key, ctrl, alt, shift, win } = &press_action {
                if !key.trim().is_empty() {
                    match actions::send_hotkey(key.trim(), *ctrl, *alt, *shift, *win) {
                        Ok(()) => log(shared, format!("крутилка {encoder}: горячая клавиша {key}")),
                        Err(error) => log(shared, format!("крутилка {encoder}: не удалось отправить сочетание {key}: {error:#}")),
                    }
                }
                return Ok(None);
            }
            let Some(target_zone) = encoder_sensor(cfg, page_index, encoder) else {
                return Ok(None);
            };
            let Some(slot_index) = target_zone.checked_sub(1).map(|v| v as usize) else {
                return Ok(None);
            };
            if slot_index >= volumes.len() {
                return Ok(None);
            }

            let app = zone_app(cfg, page_index, encoder);

            // Привязанное приложение: настоящий mute через микшер, громкость
            // при этом сохраняется.
            let (new_volume, new_muted, action) = if let Some(app) = app.as_deref() {
                let now_muted = muted[slot_index].is_none();
                muted[slot_index] = now_muted.then_some(volumes[slot_index]);
                audio.set_mute(app, now_muted);
                (
                    volumes[slot_index],
                    now_muted,
                    if now_muted { "приглушена" } else { "включена" },
                )
            } else {
                // Без привязки — прежнее локальное поведение: громкость
                // запоминается, на панели показывается 0%.
                let (new_volume, action) = match muted.get_mut(slot_index) {
                    Some(slot) => match slot.take() {
                        Some(restore) => (restore, "включена"),
                        None if volumes[slot_index] > 0 => {
                            *slot = Some(volumes[slot_index]);
                            (0, "приглушена")
                        }
                        None => {
                            *slot = Some(50);
                            (0, "приглушена")
                        }
                    },
                    None => return Ok(None),
                };
                volumes[slot_index] = new_volume;
                (new_volume, muted[slot_index].is_some(), action)
            };

            let message = format!("крутилка {encoder}: {action} → {new_volume}%");
            println!("[knob] {message}");
            log(shared, message);
            let mut state = shared_lock(shared);
            state.volumes = *volumes;
            state.muted = std::array::from_fn(|i| muted[i].is_some());

            Ok(Some(ZonePending {
                encoder: target_zone,
                volume: new_volume,
                muted: new_muted,
            }))
        }
        DeckEvent::Twist { encoder, delta } => {
            let binding = cfg.active_pages().get(page_index).and_then(|page| page.encoders.iter().find(|item| item.index == encoder))
                .map(|item| if delta >= 0 { item.clockwise } else { item.counter_clockwise })
                .unwrap_or(crate::config::EncoderBinding::Volume);
            if binding == crate::config::EncoderBinding::NoAction { return Ok(None); }
            if let crate::config::EncoderBinding::Media(action) = binding {
                match actions::send_media(action) {
                    Ok(()) => log(shared, format!("крутилка {encoder}: {}", action.label())),
                    Err(error) => log(shared, format!("крутилка {encoder}: не удалось отправить команду мультимедиа: {error:#}")),
                }
                return Ok(None);
            }
            let Some(target_zone) = encoder_sensor(cfg, page_index, encoder) else {
                return Ok(None);
            };
            let Some(slot_index) = target_zone.checked_sub(1).map(|v| v as usize) else {
                return Ok(None);
            };
            if slot_index >= volumes.len() {
                return Ok(None);
            }

            let app = zone_app(cfg, page_index, encoder);

            // Поворот снимает mute. Для приложения выключаем его в микшере,
            // для обычной зоны восстанавливаем запомненную громкость.
            let base = if let Some(app) = app.as_deref() {
                if muted[slot_index].take().is_some() {
                    audio.set_mute(app, false);
                }
                volumes[slot_index]
            } else {
                muted[slot_index]
                    .take()
                    .unwrap_or(volumes[slot_index])
            };

            let step = cfg
                .active_pages()
                .get(page_index)
                .and_then(|page| page.encoders.iter().find(|item| item.index == encoder))
                .map(|item| item.step_percent.clamp(1, 10) as i32)
                .unwrap_or(5);
            let steps = match binding {
                crate::config::EncoderBinding::VolumeUp => (delta as i32).abs(),
                crate::config::EncoderBinding::VolumeDown => -(delta as i32).abs(),
                _ => delta as i32,
            };
            let new_volume = (base + steps * step).clamp(0, 100);
            volumes[slot_index] = new_volume;
            if let Some(app) = app.as_deref() {
                audio.set_volume(app, new_volume);
            }

            let message = format!("крутилка {encoder}: {steps:+} → {new_volume}%");
            println!("[knob] {message}");
            log(shared, message);
            let mut state = shared_lock(shared);
            state.volumes = *volumes;
            state.muted = std::array::from_fn(|i| muted[i].is_some());

            Ok(Some(ZonePending {
                encoder: target_zone,
                volume: new_volume,
                muted: false,
            }))
        }
        DeckEvent::Swipe { left } => {
            let message = format!("стрип: свайп {}", if left { "влево" } else { "вправо" });
            println!("[strip] {message}");
            log(shared, message);
            Ok(None)
        }
        DeckEvent::TouchTap { zone } => {
            let message = format!("стрип: тап зоны {zone}");
            println!("[strip] {message}");
            log(shared, message);
            Ok(None)
        }
    }
}

/// Ключ приложения, привязанного к зоне (1..4).
fn zone_app(cfg: &Config, page_index: usize, encoder: u8) -> Option<String> {
    let zone = encoder_sensor(cfg, page_index, encoder)?;
    cfg.active_pages()
        .get(page_index)?
        .zones
        .iter()
        .find(|item| item.index == zone)
        .and_then(|zone| zone.app.clone())
}

/// Сенсор (1..4), которым управляет крутилка. `None` - крутилка «Без привязки»
/// и на сенсоры не влияет. Если крутилки нет в конфиге, берётся сенсор с её номером.
fn encoder_sensor(cfg: &Config, page_index: usize, encoder: u8) -> Option<u8> {
    match cfg.active_pages().get(page_index)?.encoders.iter().find(|item| item.index == encoder) {
        Some(item) => item.sensor_index,
        None => Some(encoder),
    }
}

/// Подтягивает громкость и mute из микшера для зон с приложением.
/// Возвращает `true`, если что-то изменилось и стрип нужно перерисовать.
fn sync_audio(
    cfg: &Config,
    page_index: usize,
    audio: &audio::Audio,
    volumes: &mut [i32; device::ENCODER_COUNT],
    muted: &mut [Option<i32>; device::ENCODER_COUNT],
    shared: &Arc<Mutex<Shared>>,
) -> bool {
    let state = audio.snapshot();
    let mut changed = false;

    if let Some(page) = cfg.active_pages().get(page_index) {
        for zone in &page.zones {
            let Some(app) = zone.app.as_deref() else {
                continue;
            };
            let index = zone.index.saturating_sub(1) as usize;
            if index >= volumes.len() {
                continue;
            }
            let Some(session) = state.session(app) else {
                continue;
            };

            if volumes[index] != session.volume {
                volumes[index] = session.volume;
                changed = true;
            }

            let is_muted = muted[index].is_some();
            if session.muted && !is_muted {
                muted[index] = Some(session.volume);
                changed = true;
            } else if !session.muted && is_muted {
                muted[index] = None;
                changed = true;
            }
        }
    }

    if changed {
        let mut state = shared_lock(shared);
        state.volumes = *volumes;
        state.muted = std::array::from_fn(|i| muted[i].is_some());
    }

    changed
}

/// Просит поток аудио заранее достать иконки для сохранённых приложений.
fn prime_zone_icons(audio: &audio::Audio, cfg: &Config) {
    let items: Vec<(String, String)> = cfg
        .scenes
        .iter()
        .flat_map(|scene| scene.pages.iter())
        .flat_map(|page| page.zones.iter())
        .filter_map(|zone| {
            let app = zone.app.clone()?;
            let path = zone.app_path.clone()?;
            Some((app, path))
        })
        .collect();
    audio.prime_icons(&items);
}

/// Рисует и заливает плитку зоны немедленно, минуя плановую очередь.
/// Одинаковые (вернувшиеся) значения не заливаются повторно.
async fn upload_zone(
    deck: &device::Deck,
    renderer: &Renderer,
    cfg: &Config,
    page_index: usize,
    zone: &ZonePending,
    snapshot: &Snapshot,
    audio: &AudioState,
    queue: &mut UploadQueue,
    sent: &mut HashMap<u8, u64>,
) -> Result<()> {
    let Some(zone_cfg) = cfg
        .active_pages()
        .get(page_index)
        .and_then(|page| page.zones.iter().find(|z| z.index == zone.encoder))
    else {
        return Ok(());
    };

    let Some(wire) = device::zone_index(zone_cfg.index) else {
        return Ok(());
    };

    let palette = TilePalette::resolve(cfg, &zone_cfg.colors, zone_cfg.metric);
    let img = renderer.zone_tile(zone_cfg, snapshot, zone.volume, zone.muted, audio, palette);
    let hash = hash_image(&img);

    if sent.get(&wire) == Some(&hash) {
        return Ok(());
    }

    let jpeg = mirajazz::images::convert_image_with_format(
        device::ZONE_FORMAT,
        DynamicImage::ImageRgb8(img),
    )
    .await
    .map_err(|e| anyhow!("кодирование JPEG: {e}"))?;

    deck.dev.write_image(wire, &jpeg).await?;
    deck.dev.flush().await?;

    sent.insert(wire, hash);
    queue.remove(wire);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn redraw_all(
    renderer: &Renderer,
    cfg: &Config,
    page_index: usize,
    snapshot: &Snapshot,
    volumes: &[i32; device::ENCODER_COUNT],
    muted: &[Option<i32>; device::ENCODER_COUNT],
    audio: &AudioState,
    queue: &mut UploadQueue,
    sent: &mut HashMap<u8, u64>,
    last_render: &mut HashMap<u8, Instant>,
    zones_due: bool,
    now: Instant,
) -> Result<()> {
    let page = cfg
        .active_pages()
        .get(page_index)
        .ok_or_else(|| anyhow!("в конфиге нет ни одной страницы"))?;

    let global_interval = Duration::from_millis(cfg.monitor.interval_ms.max(MIN_METRICS_MS));
    let refresh = refresh_gap(cfg);

    for key in &page.keys {
        let index = key.index();
        let Some(wire) = device::wire_index(index) else {
            continue;
        };

        let interval = match key {
            KeyCfg::Metric {
                interval_ms: Some(ms),
                ..
            } => Duration::from_millis((*ms).max(MIN_METRICS_MS)).max(refresh),
            KeyCfg::Metric { .. } => global_interval.max(refresh),
            // Статичные плитки рисуются один раз: при подключении, смене страницы или конфига.
            _ => Duration::MAX,
        };

        let due = last_render
            .get(&index)
            .map(|t| now.duration_since(*t) >= interval)
            .unwrap_or(true);

        if !due {
            continue;
        }

        let metric = match key {
            KeyCfg::Metric { metric, .. } => Some(*metric),
            _ => None,
        };
        let palette = TilePalette::resolve(cfg, key.colors(), metric);
        let img = match key {
            KeyCfg::Metric { metric, style, battery_device, .. } => {
                renderer.key_tile(*metric, *style, snapshot, palette, battery_device.as_deref())
            }
            KeyCfg::NextPage { .. } => renderer.message_tile("PAGE", "→", Rgb(palette.digit)),
            KeyCfg::Launch { path, .. } => renderer.message_tile("ОТКРЫТЬ", path, Rgb(palette.digit)),
            KeyCfg::Hotkey { key, .. } => renderer.message_tile("КЛАВИША", key, Rgb(palette.digit)),
            KeyCfg::Media { action, .. } => renderer.message_tile("MEDIA", action.label(), Rgb(palette.digit)),
        };

        upload_if_changed(wire, img, device::KEY_FORMAT, queue, sent).await?;
        last_render.insert(index, now);
    }

    // Незанятые ячейки очищаются, иначе после смены страницы или сцены на
    // панели остаётся картинка прежней страницы.
    for index in 1..=device::KEY_COUNT as u8 {
        if last_render.contains_key(&index) || page.keys.iter().any(|key| key.index() == index) {
            continue;
        }
        let Some(wire) = device::wire_index(index) else {
            continue;
        };
        let img = renderer.blank_tile(crate::render::KEY_W, crate::render::KEY_H);
        upload_if_changed(wire, img, device::KEY_FORMAT, queue, sent).await?;
        last_render.insert(index, now);
    }
    for index in 1..=device::ZONE_COUNT as u8 {
        let render_key = BLANK_ZONE_KEY + index;
        let has_content = page.zones.iter().any(|zone| zone.index == index && zone.has_content());
        if last_render.contains_key(&render_key) || has_content {
            continue;
        }
        let Some(wire) = device::zone_index(index) else {
            continue;
        };
        let img = renderer.blank_tile(crate::render::ZONE_W, crate::render::ZONE_H);
        upload_if_changed(wire, img, device::ZONE_FORMAT, queue, sent).await?;
        last_render.insert(render_key, now);
    }

    if zones_due {
        for zone in &page.zones {
            if !zone.has_content() {
                continue;
            }
            let Some(wire) = device::zone_index(zone.index) else {
                continue;
            };

            let volume = zone
                .index
                .checked_sub(1)
                .and_then(|i| volumes.get(i as usize))
                .copied()
                .unwrap_or(50);
            let is_muted = muted
                .get(zone.index.saturating_sub(1) as usize)
                .is_some_and(|slot| slot.is_some());

            let palette = TilePalette::resolve(cfg, &zone.colors, zone.metric);
            let img = renderer.zone_tile(zone, snapshot, volume, is_muted, audio, palette);
            upload_if_changed(wire, img, device::ZONE_FORMAT, queue, sent).await?;
        }
    }

    Ok(())
}

async fn upload_if_changed(
    wire: u8,
    img: RgbImage,
    format: ImageFormat,
    queue: &mut UploadQueue,
    sent: &mut HashMap<u8, u64>,
) -> Result<()> {
    upload_if_changed_impl(wire, img, format, queue, sent).await
}

async fn upload_if_changed_impl(
    wire: u8,
    img: RgbImage,
    format: ImageFormat,
    queue: &mut UploadQueue,
    sent: &mut HashMap<u8, u64>,
) -> Result<()> {
    let hash = hash_image(&img);
    if sent.get(&wire) == Some(&hash) {
        return Ok(());
    }

    let jpeg = mirajazz::images::convert_image_with_format(format, DynamicImage::ImageRgb8(img))
        .await
        .map_err(|e| anyhow!("кодирование JPEG: {e}"))?;

    queue.push(wire, jpeg);
    sent.insert(wire, hash);

    Ok(())
}

fn hash_image(img: &RgbImage) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in img.as_raw() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn shared_lock(shared: &Arc<Mutex<Shared>>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn log(shared: &Arc<Mutex<Shared>>, message: String) {
    shared_lock(shared).push_log(message);
}

fn status(shared: &Arc<Mutex<Shared>>, message: String) {
    shared_lock(shared).status = message;
}

/// Очередь заливки: каждая плитка хранится в единственном экземпляре,
/// повторное изменение заменяет данные, а не добавляет новую задачу.
#[derive(Default)]
struct UploadQueue {
    order: VecDeque<u8>,
    queued: HashSet<u8>,
    data: HashMap<u8, Vec<u8>>,
}

impl UploadQueue {
    fn push(&mut self, wire: u8, jpeg: Vec<u8>) {
        self.data.insert(wire, jpeg);
        if self.queued.insert(wire) {
            self.order.push_back(wire);
        }
    }

    fn pop(&mut self) -> Option<(u8, Vec<u8>)> {
        while let Some(wire) = self.order.pop_front() {
            self.queued.remove(&wire);
            if let Some(data) = self.data.remove(&wire) {
                return Some((wire, data));
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    /// Убирает ещё не отправленную плитку: её уже залили напрямую.
    fn remove(&mut self, wire: u8) {
        self.data.remove(&wire);
        if self.queued.remove(&wire) {
            self.order.retain(|w| *w != wire);
        }
    }
}
