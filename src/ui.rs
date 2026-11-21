//! Окно конфигурации (egui/eframe).
//!
//! Устроено как «зеркало» устройства: 2×5 плиток клавиш и полоса из четырёх
//! зон стрипа рисуются тем же [`Renderer`], что и идущие на устройство
//! картинки, поэтому значения в окне совпадают с экраном панели в реальном
//! времени.
//!
//! Настройка ячейки открывается выдвижной панелью справа и только по клику
//! на плитку (или на крутилку под зоной). Правки попадают в черновик,
//! «Применить» пишет `config.toml` — цикл устройства подхватывает файл сам.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::epaint::text::{FontData, FontDefinitions};
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, Margin, Pos2, Rect, Sense,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2,
};
use image::Rgb;

use crate::config::{self, Config, EncoderBinding, EncoderPressAction, KeyCfg, MediaAction, Metric, Page, Style, TileAction, TileColors, ZoneCfg};
use crate::monitor::{gib, Snapshot};
use crate::render::{Renderer, TilePalette, KEY_W, ZONE_H, ZONE_W};
use crate::shared::{AudioState, Shared};

// ---------------------------------------------------------------------------
// Палитра
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(11, 13, 18);
const HEADER: Color32 = Color32::from_rgb(14, 17, 23);
const PANEL: Color32 = Color32::from_rgb(16, 19, 26);
const CARD: Color32 = Color32::from_rgb(20, 24, 32);
const CARD_2: Color32 = Color32::from_rgb(26, 31, 41);
const CARD_3: Color32 = Color32::from_rgb(34, 40, 52);
const BORDER: Color32 = Color32::from_rgb(39, 46, 60);
const TEXT: Color32 = Color32::from_rgb(232, 236, 243);
const MUTED: Color32 = Color32::from_rgb(140, 149, 168);
const FAINT: Color32 = Color32::from_rgb(96, 105, 124);
const ACCENT: Color32 = Color32::from_rgb(74, 158, 255);
const ACCENT_DARK: Color32 = Color32::from_rgb(23, 49, 84);
const ACCENT_TEXT: Color32 = Color32::from_rgb(154, 205, 255);
const GREEN: Color32 = Color32::from_rgb(53, 208, 127);
const AMBER: Color32 = Color32::from_rgb(255, 194, 74);
const RED: Color32 = Color32::from_rgb(255, 92, 92);
const CHASSIS: Color32 = Color32::from_rgb(25, 29, 38);
const CHASSIS_STROKE: Color32 = Color32::from_rgb(44, 51, 66);
const SCREEN: Color32 = Color32::from_rgb(9, 11, 15);

/// Пауза после последнего нажатия клавиши в поле ввода перед сохранением конфига.
const APPLY_DELAY: Duration = Duration::from_millis(400);

const CPU_METRICS: [Metric; 5] = [Metric::CpuLoad, Metric::CpuTemp, Metric::CpuFreq, Metric::CpuMaxCore, Metric::CpuThreads];
const MEMORY_METRICS: [Metric; 3] = [Metric::RamPercent, Metric::RamUsed, Metric::RamTotal];
const GPU_METRICS: [Metric; 5] = [Metric::GpuLoad, Metric::GpuTemp, Metric::GpuVram, Metric::GpuVramPercent, Metric::GpuFan];
const STORAGE_NETWORK_METRICS: [Metric; 5] = [Metric::NetDown, Metric::NetUp, Metric::DiskUsed, Metric::DiskActivity, Metric::DiskUsedGb];
const SYSTEM_METRICS: [Metric; 2] = [Metric::Clock, Metric::Uptime];
const DEVICE_METRICS: [Metric; 1] = [Metric::DeviceBattery];

fn metric_groups() -> [(&'static str, &'static [Metric]); 6] {
    [
        ("Процессор", &CPU_METRICS),
        ("Память", &MEMORY_METRICS),
        ("Видеокарта", &GPU_METRICS),
        ("Сеть и накопители", &STORAGE_NETWORK_METRICS),
        ("Система", &SYSTEM_METRICS),
        ("Устройства", &DEVICE_METRICS),
    ]
}

fn metric_icon(metric: Metric) -> IconKind {
    match metric {
        Metric::CpuTemp | Metric::CpuLoad | Metric::CpuFreq | Metric::CpuMaxCore | Metric::CpuThreads => IconKind::Cpu,
        Metric::RamPercent | Metric::RamUsed | Metric::RamTotal => IconKind::Memory,
        Metric::GpuTemp | Metric::GpuLoad | Metric::GpuVram | Metric::GpuVramPercent | Metric::GpuFan => IconKind::Gpu,
        Metric::NetDown | Metric::NetUp => IconKind::Network,
        Metric::DiskUsed | Metric::DiskActivity | Metric::DiskUsedGb => IconKind::Disk,
        Metric::Clock | Metric::Uptime => IconKind::System,
        Metric::DeviceBattery => IconKind::Battery,
    }
}

fn metric_available(metric: Metric, snapshot: &Snapshot) -> bool {
    match metric {
        Metric::CpuTemp => snapshot.cpu_temp.is_some(),
        Metric::CpuFreq => snapshot.cpu_freq_mhz > 0,
        Metric::GpuTemp | Metric::GpuLoad | Metric::GpuVram | Metric::GpuFan => snapshot.gpu.is_some(),
        Metric::GpuVramPercent => snapshot.gpu.as_ref().is_some_and(|gpu| gpu.mem_total > 0),
        Metric::DiskUsed => snapshot.disk_percent.is_some(),
        Metric::DiskActivity => snapshot.disk_activity.is_some(),
        Metric::DiskUsedGb => snapshot.disk_total > 0,
        Metric::DeviceBattery => true,
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Оформление egui
// ---------------------------------------------------------------------------

/// Шрифты с кириллицей + тёмная тема приложения.
pub fn setup(ctx: &egui::Context) {
    setup_fonts(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.animation_time = 0.14;
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.spacing.interact_size = Vec2::new(40.0, 30.0);
        style.spacing.slider_width = 340.0;
        style.spacing.combo_width = 160.0;
        style.visuals = dark_visuals();
    });
}

/// Ставит системный шрифт с кириллицей (у egui встроенный её не содержит).
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let fonts_dir = {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        std::path::PathBuf::from(windir).join("Fonts")
    };
    for (name, file) in [
        ("segoeui", "segoeui.ttf"),
        ("seguisb", "seguisb.ttf"),
        ("segoeuib", "segoeuib.ttf"),
    ] {
        let path = fonts_dir.join(file);
        if let Ok(bytes) = std::fs::read(&path) {
            fonts
                .font_data
                .insert(name.to_string(), FontData::from_owned(bytes).into());
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.insert(0, name.to_string());
            }
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                family.insert(0, name.to_string());
            }
        }
    }

    ctx.set_fonts(fonts);
}

fn dark_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(9, 11, 16);
    v.faint_bg_color = CARD;
    v.text_edit_bg_color = Some(Color32::from_rgb(11, 14, 20));
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = AMBER;
    v.error_fg_color = RED;
    v.selection.bg_fill = ACCENT_DARK;
    v.selection.stroke = Stroke::new(1.0, ACCENT_TEXT);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(12);
    v.widgets.noninteractive = widget_visual(CARD, CARD, BORDER, TEXT);
    v.widgets.inactive = widget_visual(CARD_2, CARD_2, BORDER, TEXT);
    v.widgets.hovered = widget_visual(
        CARD_3,
        CARD_3,
        Color32::from_rgb(62, 73, 92),
        Color32::WHITE,
    );
    v.widgets.active = widget_visual(ACCENT_DARK, ACCENT_DARK, ACCENT, ACCENT_TEXT);
    v.widgets.open = widget_visual(CARD_3, CARD_3, BORDER, TEXT);
    v
}

fn widget_visual(bg: Color32, weak: Color32, border: Color32, fg: Color32) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak,
        bg_stroke: Stroke::new(1.0, border),
        corner_radius: CornerRadius::same(9),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    }
}

// ---------------------------------------------------------------------------
// Состояние окна
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selection {
    Key(u8),
    Zone(u8),
    Encoder(u8),
    /// Свайпы влево/вправо по тач-стрипу.
    StripSwipe,
    Device,
    Library,
}
#[derive(Clone, Copy)]
enum DeleteTarget { Key(u8), Zone(u8), Page(usize), Scene(usize) }
#[derive(Clone, Copy)]
struct SceneDrag(usize);
#[derive(Clone, Copy)]
struct PageDrag(usize);
#[derive(Clone, Copy)]
enum CellDrag { Key(u8), Zone(u8) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LibraryTab { Button, Sensor }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsTab { General, Mixers, Log }

#[derive(Clone)]
enum ComponentPayload {
    Key(KeyTemplate),
    SensorAppVolume,
    SensorMetric(Metric),
}

#[derive(Clone)]
enum KeyTemplate {
    Metric(Metric),
    NextPage,
    Launch,
    Hotkey,
    Media,
}

struct CachedTex {
    hash: u64,
    texture: TextureHandle,
}

#[derive(Default)]
struct Tiles {
    key: [Option<CachedTex>; 10],
    zone: [Option<CachedTex>; 4],
}

pub struct ConfigApp {
    shared: Arc<Mutex<Shared>>,
    draft: Config,
    saved_config: Config,
    undo_stack: Vec<Config>,
    selected_scene: usize,
    selected_page: usize,
    dirty: bool,
    save_error: Option<String>,
    renderer: Option<Renderer>,
    tiles: Tiles,
    style_previews: [Option<CachedTex>; 3],
    style_key: Option<(Metric, u64)>,
    /// Устройство заряда для кэша превью оформления.
    style_battery_tag: Option<String>,
    selected: Option<Selection>,
    inspector_open: bool,
    tex_dirty: bool,
    last_snap_hash: u64,
    library_tab: LibraryTab,
    confirm_delete: Option<DeleteTarget>,
    renaming_page: Option<usize>,
    renaming_scene: Option<usize>,
    global_settings: bool,
    settings_tab: SettingsTab,
    audio_icon_textures: HashMap<String, TextureHandle>,
    cell_drop_handled: bool,
    /// Когда сохранить изменения, набранные с клавиатуры.
    apply_at: Option<Instant>,
    /// Окно спрятано в трей.
    hidden: bool,
    /// Трей попросил завершить приложение: закрытие больше не отменяем.
    quitting: bool,
    /// Последний проход динамического захвата в окне.
    last_dynamic_at: Instant,
}

impl ConfigApp {
    pub fn new(shared: Arc<Mutex<Shared>>) -> Self {
        let (draft, selected_scene, selected_page) = shared
            .lock()
            .map(|s| (s.config.clone(), s.active_scene, s.active_page))
            .unwrap_or_else(|p| {
                let s = p.into_inner();
                (s.config.clone(), s.active_scene, s.active_page)
            });

        let renderer = match Renderer::new() {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("[ui] превью недоступно: {e}");
                None
            }
        };
        let selected_scene = selected_scene.min(draft.scenes.len().saturating_sub(1));
        let selected_page = selected_page.min(draft.scenes.get(selected_scene)
            .map(|scene| scene.pages.len().saturating_sub(1)).unwrap_or(0));
        let saved_config = draft.clone();

        Self {
            shared,
            draft,
            saved_config,
            undo_stack: Vec::new(),
            selected_scene,
            selected_page,
            dirty: false,
            save_error: None,
            renderer,
            tiles: Tiles::default(),
            style_previews: std::array::from_fn(|_| None),
            style_key: None,
            style_battery_tag: None,
            selected: Some(Selection::Library),
            inspector_open: true,
            tex_dirty: true,
            last_snap_hash: 0,
            library_tab: LibraryTab::Button,
            confirm_delete: None,
            renaming_page: None,
            renaming_scene: None,
            global_settings: false,
            settings_tab: SettingsTab::General,
            audio_icon_textures: HashMap::new(),
            cell_drop_handled: false,
            apply_at: None,
            hidden: false,
            quitting: false,
            last_dynamic_at: Instant::now() - Duration::from_secs(5),
        }
    }

    // -- общие действия ----------------------------------------------------

    /// Открывает настройки клавиши сразу при запуске (для офлайн-превью).
    #[allow(dead_code)]
    pub fn preview_open_key(&mut self, index: u8) {
        self.selected = Some(Selection::Key(index));
        self.inspector_open = true;
    }

    fn apply(&mut self) {
        if !self.dirty { return; }
        if let Err(error) = config::try_save(&self.draft) {
            self.save_error = Some(format!("Не удалось сохранить настройки: {error}"));
            return;
        }
        self.undo_stack.push(self.saved_config.clone());
        if self.undo_stack.len() > 64 { self.undo_stack.remove(0); }
        if let Ok(mut shared) = self.shared.lock() {
            shared.config = self.draft.clone();
            shared.active_scene = self.selected_scene;
            shared.active_page = self.selected_page.min(self.pages().len().saturating_sub(1));
            shared.config_revision = shared.config_revision.wrapping_add(1);
            shared.config_error = None;
            shared.push_log("конфигурация применена".to_string());
        }
        self.saved_config = self.draft.clone();
        self.save_error = None;
        self.dirty = false;
        self.apply_at = None;
    }

    fn undo(&mut self) {
        let Some(previous) = self.undo_stack.pop() else { return; };
        if let Err(error) = config::try_save(&previous) {
            self.save_error = Some(format!("Не удалось отменить изменение: {error}"));
            self.undo_stack.push(previous);
            return;
        }
        self.draft = previous.clone();
        self.saved_config = previous.clone();
        self.selected_scene = previous.active_scene.min(previous.scenes.len().saturating_sub(1));
        self.selected_page = self.selected_page.min(self.pages().len().saturating_sub(1));
        self.tex_dirty = true;
        self.dirty = false;
        if let Ok(mut shared) = self.shared.lock() {
            shared.config = previous;
            shared.active_scene = self.selected_scene;
            shared.active_page = self.selected_page;
            shared.config_revision = shared.config_revision.wrapping_add(1);
            shared.push_log("изменение отменено".to_string());
        }
    }

    /// Обрабатывает просьбы трея и прячет окно вместо закрытия.
    /// Возвращает `true`, пока окно спрятано и рисовать его не нужно.
    fn handle_tray(&mut self, ctx: &egui::Context) -> bool {
        let (hide_to_tray, show, quit) = {
            let mut shared = match self.shared.lock() {
                Ok(shared) => shared,
                Err(poisoned) => poisoned.into_inner(),
            };
            (shared.hide_to_tray, std::mem::take(&mut shared.tray_show), std::mem::take(&mut shared.tray_quit))
        };
        if quit {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if show && !self.quitting {
            self.hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if hide_to_tray && !self.quitting && ctx.input(|input| input.viewport().close_requested()) {
            self.apply();
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }
        self.hidden
    }

    fn discard(&mut self) {
        if let Ok(shared) = self.shared.lock() {
            self.draft = shared.config.clone();
            self.selected_scene = shared.active_scene.min(self.draft.scenes.len().saturating_sub(1));
            self.draft.active_scene = self.selected_scene;
            self.selected_page = shared.active_page.min(self.pages().len().saturating_sub(1));
        }
        self.dirty = false;
        self.tex_dirty = true;
        self.style_key = None;
    }

    fn touch(&mut self, resp: &egui::Response) {
        if resp.changed() {
            self.dirty = true;
            self.tex_dirty = true;
        }
    }

    fn open_cell(&mut self, selection: Selection) {
        self.selected = Some(selection);
        self.inspector_open = true;
    }

    // -- конфиг ------------------------------------------------------------

    fn page(&self) -> Option<&crate::config::Page> {
        self.pages().get(self.selected_page)
    }

    fn pages(&self) -> &[Page] {
        self.draft.active_pages()
    }

    fn pages_mut(&mut self) -> Option<&mut Vec<Page>> {
        self.draft.active_scene_mut().map(|scene| &mut scene.pages)
    }

    fn page_mut(&mut self) -> Option<&mut Page> {
        let index = self.selected_page;
        self.pages_mut()?.get_mut(index)
    }

    fn key_cfg(&self, index: u8) -> Option<&KeyCfg> {
        self.page()?.keys.iter().find(|k| k.index() == index)
    }

    fn key_cfg_mut(&mut self, index: u8) -> Option<&mut KeyCfg> {
        self.page_mut()?
            .keys
            .iter_mut()
            .find(|k| k.index() == index)
    }

    fn ensure_key(&mut self, index: u8) {
        let page = match self.page_mut() {
            Some(p) => p,
            None => return,
        };
        if page.keys.iter().any(|k| k.index() == index) {
            return;
        }
        page.keys.push(KeyCfg::Metric {
            index,
            metric: Metric::CpuLoad,
            style: Style::Value,
            interval_ms: None,
            battery_device: None,
            on_press: crate::config::TileAction::NoAction,
            colors: TileColors::default(),
        });
        page.keys.sort_by_key(|k| k.index());
        self.dirty = true;
        self.tex_dirty = true;
    }

    fn ensure_zone(&mut self, index: u8) {
        let page = match self.page_mut() {
            Some(p) => p,
            None => return,
        };
        if page.zones.iter().any(|z| z.index == index) {
            return;
        }
        page.zones.push(ZoneCfg {
            index,
            title: format!("VOL {index}"),
            metric: None,
            style: Style::Value,
            app: None,
            app_path: None,
            dynamic: false,
            step_percent: 5,
            press_action: crate::config::EncoderPressAction::MuteApplication,
            tap_action: crate::config::TileAction::NoAction,
            battery_device: None,
            colors: TileColors::default(),
        });
        page.zones.sort_by_key(|z| z.index);
        self.dirty = true;
        self.tex_dirty = true;
    }

    // -- текстуры ----------------------------------------------------------

    fn refresh_tiles(
        &mut self,
        ctx: &egui::Context,
        snapshot: &Snapshot,
        volumes: [i32; 4],
        muted: [bool; 4],
        audio: &AudioState,
    ) -> bool {
        let hash = snapshot_hash(snapshot, volumes, muted, audio);
        if !self.tex_dirty && hash == self.last_snap_hash {
            return false;
        }
        self.tex_dirty = false;
        self.last_snap_hash = hash;
        self.style_key = None;

        let Some(renderer) = &self.renderer else {
            return false;
        };
        let Some(page) = self.pages().get(self.selected_page).cloned() else {
            return false;
        };

        for slot in 1..=10u8 {
            let img = match page.keys.iter().find(|k| k.index() == slot) {
                Some(KeyCfg::Metric { metric, style, colors, battery_device, .. }) => {
                    let palette = TilePalette::resolve(&self.draft, colors, Some(*metric));
                    renderer.key_tile(*metric, *style, snapshot, palette, battery_device.as_deref())
                }
                Some(key) => {
                    let palette = TilePalette::resolve(&self.draft, key.colors(), None);
                    match key {
                        KeyCfg::NextPage { .. } => renderer.message_tile("PAGE", "→", Rgb(palette.digit)),
                        KeyCfg::Launch { path, .. } => renderer.message_tile("ОТКРЫТЬ", path, Rgb(palette.digit)),
                        KeyCfg::Hotkey { key, .. } => renderer.message_tile("КЛАВИША", key, Rgb(palette.digit)),
                        KeyCfg::Media { action, .. } => renderer.message_tile("MEDIA", action.label(), Rgb(palette.digit)),
                        KeyCfg::Metric { .. } => unreachable!(),
                    }
                }
                None => renderer.blank_tile(KEY_W, KEY_W),
            };
            put_texture(
                ctx,
                &mut self.tiles.key[slot as usize - 1],
                format!("tile-key-{slot}"),
                img,
            );
        }

        for slot in 1..=4u8 {
            let zone = page.zones.iter().find(|z| z.index == slot);
            let volume = volumes
                .get(slot as usize - 1)
                .copied()
                .unwrap_or(50);
            let is_muted = muted
                .get(slot as usize - 1)
                .copied()
                .unwrap_or(false);
            let img = match zone {
                Some(zone) if zone.has_content() => {
                    let palette = TilePalette::resolve(&self.draft, &zone.colors, zone.metric);
                    renderer.zone_tile(zone, snapshot, volume, is_muted, audio, palette)
                }
                _ => renderer.blank_tile(ZONE_W, ZONE_H),
            };
            put_texture(
                ctx,
                &mut self.tiles.zone[slot as usize - 1],
                format!("tile-zone-{slot}"),
                img,
            );
        }

        true
    }

    fn refresh_audio_icons(&mut self, ctx: &egui::Context, audio: &AudioState) {
        for (app, icon) in &audio.icons {
            if !self.audio_icon_textures.contains_key(app) {
                let size = [icon.width() as usize, icon.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, icon.as_raw());
                self.audio_icon_textures.insert(app.clone(), ctx.load_texture(format!("audio-{app}"), color, TextureOptions::LINEAR));
            }
        }
    }

    fn ensure_style_previews(
        &mut self,
        ctx: &egui::Context,
        metric: Metric,
        snapshot: &Snapshot,
        battery_device: Option<&str>,
    ) {
        let key = (
            metric,
            snapshot_hash(snapshot, [0; 4], [false; 4], &AudioState::default()),
        );
        let cache_tag = battery_device.unwrap_or("").to_string();
        if self.style_key == Some(key) && self.style_battery_tag.as_deref() == Some(cache_tag.as_str())
        {
            return;
        }
        self.style_key = Some(key);
        self.style_battery_tag = Some(cache_tag);

        let Some(renderer) = &self.renderer else { return };
        let palette = TilePalette::resolve(&self.draft, &TileColors::default(), Some(metric));

        let mut preview_snap = snapshot.clone();
        let preview_battery = if metric == Metric::DeviceBattery {
            let id = battery_device
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| "__preview__".into());
            if !preview_snap.batteries.iter().any(|d| d.id == id) {
                preview_snap.batteries.push(crate::monitor::BatteryDevice {
                    id: id.clone(),
                    name: "DEVICE".into(),
                    kind: crate::monitor::BatteryKind::Mouse,
                    percent: Some(72),
                    charging: false,
                    connected: true,
                });
            }
            Some(id)
        } else {
            None
        };

        for (i, style) in Style::ALL.iter().enumerate() {
            let img = renderer.key_tile(
                metric,
                *style,
                &preview_snap,
                palette,
                preview_battery.as_deref(),
            );
            put_texture(
                ctx,
                &mut self.style_previews[i],
                format!("style-{i}"),
                img,
            );
        }
    }

    // -- шапка и подвал ----------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui, snapshot: Option<&Snapshot>, status: &str) {
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(HEADER)
                    .inner_margin(Margin::symmetric(20, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    logo(ui);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("AKP Deck")
                                .size(16.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            egui::RichText::new("AKP05E · панель управления")
                                .size(10.5)
                                .color(FAINT),
                        );
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_chip(ui, status);

                        let mut page_choice = self.selected_page;
                        egui::ComboBox::from_id_salt("page-selector")
                            .selected_text(
                                self.page()
                                    .map(|page| page.name.as_str())
                                    .unwrap_or("Страница"),
                            )
                            .width(118.0)
                            .show_ui(ui, |ui| {
                                for (index, page) in self.pages().iter().enumerate() {
                                    ui.selectable_value(&mut page_choice, index, &page.name);
                                }
                            });
                        if page_choice != self.selected_page {
                            self.selected_page = page_choice;
                            self.dirty = true;
                            self.tex_dirty = true;
                            self.style_key = None;
                            self.selected = None;
                            self.inspector_open = false;
                        }
                        if ui.small_button("+").on_hover_text("Добавить страницу").clicked() {
                            let next_number = self.pages().len() + 1;
                            if let Some(pages) = self.pages_mut() {
                                pages.push(Page {
                                    name: format!("Страница {next_number}"),
                                    ..Page::default()
                                });
                            }
                            self.selected_page = self.pages().len().saturating_sub(1);
                            self.dirty = true;
                            self.tex_dirty = true;
                            self.selected = None;
                            self.inspector_open = false;
                        }
                        if self.pages().len() > 1
                            && ui.small_button("−").on_hover_text("Удалить выбранную страницу").clicked()
                        {
                            let index = self.selected_page;
                            if let Some(pages) = self.pages_mut() { pages.remove(index); }
                            self.selected_page = self.selected_page.min(self.pages().len().saturating_sub(1));
                            self.dirty = true;
                            self.tex_dirty = true;
                            self.selected = None;
                            self.inspector_open = false;
                        }

                        let device_selected = matches!(self.selected, Some(Selection::Device))
                            && self.inspector_open;
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Устройство").size(12.5),
                                )
                                .selected(device_selected),
                            )
                            .clicked()
                        {
                            self.open_cell(Selection::Device);
                        }

                        ui.add_space(4.0);

                    });
                });
            });
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(HEADER)
                    .inner_margin(Margin::symmetric(20, 8)),
            )
            .show(ui, |ui| {
                if let Some(error) = &self.save_error {
                    ui.label(egui::RichText::new(error).size(11.0).color(RED));
                    ui.add_space(4.0);
                }
                ui.horizontal(|ui| {
                    let (color, text) = if self.dirty {
                        (AMBER, "есть несохранённые изменения")
                    } else {
                        (GREEN, "все изменения применены")
                    };
                    dot(ui, color, 5.0);
                    ui.label(egui::RichText::new(text).size(11.0).color(MUTED));

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("Esc — закрыть панель · Ctrl+S — применить")
                                .size(10.5)
                                .color(FAINT),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "config.toml · {}",
                                config::config_path().display()
                            ))
                            .size(10.0)
                            .color(FAINT),
                        );
                    });
                });
            });
    }

    // -- центральная часть -------------------------------------------------

    fn central(&mut self, ui: &mut egui::Ui, _snapshot: Option<&Snapshot>, volumes: [i32; 4], muted: [bool; 4]) {
        egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(16, 8)).show(ui, |ui| {
            let device_height = (5.3 * ((ui.available_width() - 32.0) / 7.15).clamp(40.0, 92.0)) + 50.0;
            let vertical_space = (ui.available_height() - device_height - 54.0).max(0.0) * 0.5;
            ui.add_space(vertical_space);
            egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(18)).inner_margin(Margin::same(8)).show(ui, |ui| self.device_card(ui, volumes, muted));
            ui.add_space(12.0);
            let row_width = ui.available_width();
            ui.allocate_ui_with_layout(Vec2::new(row_width, 40.0), Layout::top_down(Align::Center), |ui| {
              ui.horizontal(|ui| {
                let count = self.pages().len();
                for idx in 0..count {
                    let stored_name = self.pages()[idx].name.clone();
                    let name = if stored_name.starts_with("Страница ") { (idx + 1).to_string() } else { stored_name };
                    // Перетаскивание начинается только после сдвига мыши, поэтому клик переключает страницу.
                    let response = ui.add(egui::Button::new(name).selected(idx == self.selected_page).sense(Sense::click_and_drag()));
                    if response.drag_started() { egui::DragAndDrop::set_payload(ui.ctx(), PageDrag(idx)); }
                    if response.dragged() { ui.ctx().set_cursor_icon(CursorIcon::Grabbing); }
                    if egui::DragAndDrop::payload::<PageDrag>(ui.ctx()).is_some_and(|payload| payload.0 != idx) && response.contains_pointer() {
                        ui.painter().rect_stroke(response.rect, CornerRadius::same(6), Stroke::new(1.5, ACCENT), StrokeKind::Inside);
                    }
                    if response.clicked() { self.selected_page = idx; self.draft.active_scene = self.selected_scene; self.dirty = true; self.tex_dirty = true; self.selected = Some(Selection::Library); }
                    if let Some(payload) = response.dnd_release_payload::<PageDrag>() {
                        let from = payload.0;
                        if from < self.pages().len() && from != idx {
                            if let Some(pages) = self.pages_mut() { let page = pages.remove(from); pages.insert(idx, page); }
                            self.selected_page = if self.selected_page == from { idx } else if from < self.selected_page && idx >= self.selected_page { self.selected_page - 1 } else if from > self.selected_page && idx <= self.selected_page { self.selected_page + 1 } else { self.selected_page };
                            self.draft.active_scene = self.selected_scene; self.dirty = true; self.tex_dirty = true;
                        }
                    }
                    response.context_menu(|ui| {
                        ui.spacing_mut().button_padding.x = 4.0;
                        if ui.button("Дублировать страницу").clicked() && self.pages().len() < 8 {
                            let mut page = self.pages()[idx].clone(); page.name = format!("{} копия", page.name);
                            self.pages_mut().unwrap().insert(idx + 1, page); self.selected_page = idx + 1; self.dirty = true; self.tex_dirty = true; ui.close();
                        }
                        if ui.button("Переименовать страницу").clicked() { self.renaming_page = Some(idx); ui.close(); }
                        if idx > 0 && ui.button("Удалить страницу").clicked() { self.confirm_delete = Some(DeleteTarget::Page(idx)); ui.close(); }
                    });
                }
                if count < 8 && plus_icon_button(ui).on_hover_text("Добавить страницу").clicked() {
                    let idx = count + 1; self.pages_mut().unwrap().push(Page { name: format!("Страница {idx}"), ..Page::default() });
                    self.selected_page = count; self.dirty = true; self.tex_dirty = true;
                }
              });
            });
            ui.add_space(4.0);
            if let Some(idx) = self.renaming_page {
                if let Some(page) = self.pages_mut().and_then(|pages| pages.get_mut(idx)) {
                    let mut name = page.name.clone();
                    let response = ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY));
                    if response.changed() { page.name = name; self.dirty = true; }
                    if response.lost_focus() { self.renaming_page = None; }
                }
            }
        });
    }

    // -- макет устройства --------------------------------------------------

    fn device_card(
        &mut self,
        ui: &mut egui::Ui,
        volumes: [i32; 4],
        muted: [bool; 4],
    ) {
        let keys = self.page().map(|p| p.keys.clone()).unwrap_or_default();
        let zones = self.page().map(|p| p.zones.clone()).unwrap_or_default();
        let encoders = self.page().map(|p| p.encoders.clone()).unwrap_or_default();

        let avail_w = ui.available_width().max(320.0);
        let unit = (avail_w / 7.15).clamp(40.0, 92.0);
        let body_w = 6.9 * unit;
        let body_h = 5.3 * unit;

        let (outer, _) = ui.allocate_exact_size(
            Vec2::new(avail_w, body_h + 26.0),
            Sense::hover(),
        );
        let body = Rect::from_center_size(
            Pos2::new(outer.center().x, outer.top() + 8.0 + body_h / 2.0),
            Vec2::new(body_w, body_h),
        );

        let painter = ui.painter().clone();
        let clip = painter.with_clip_rect(outer);

        // Мягкое свечение под корпусом.
        let glow = body.center();
        clip.circle_filled(glow, body_w * 0.46, Color32::from_rgba_unmultiplied(74, 158, 255, 7));
        clip.circle_filled(glow, body_w * 0.34, Color32::from_rgba_unmultiplied(74, 158, 255, 6));

        // Тень.
        for i in (1..=7).rev() {
            let grow = i as f32 * 1.8;
            clip.rect(
                body.expand(grow).translate(Vec2::new(0.0, 5.0)),
                CornerRadius::same((24 + grow as i32).min(255) as u8),
                Color32::from_black_alpha(9),
                Stroke::NONE,
                StrokeKind::Outside,
            );
        }

        // Корпус.
        clip.rect(
            body,
            CornerRadius::same((0.22 * unit) as u8),
            CHASSIS,
            Stroke::new(1.0, CHASSIS_STROKE),
            StrokeKind::Inside,
        );
        clip.rect(
            body.shrink(1.5),
            CornerRadius::same((0.21 * unit) as u8),
            Color32::TRANSPARENT,
            Stroke::new(1.0, Color32::from_white_alpha(5)),
            StrokeKind::Inside,
        );

        // Логотип.
        clip.text(
            Pos2::new(body.center().x, body.top() + 0.33 * unit),
            Align2::CENTER_CENTER,
            "A K P   D E C K",
            FontId::proportional((0.135 * unit).max(9.0)),
            Color32::from_rgb(96, 106, 126),
        );

        // ---- клавиши ----
        let gap = 0.09 * unit;
        let grid_w = 5.0 * unit + 4.0 * gap;
        let grid_left = body.center().x - grid_w / 2.0;
        let grid_top = body.top() + 0.62 * unit;
        let radius = (0.14 * unit) as u8;

        for slot in 1..=10u8 {
            let col = ((slot - 1) % 5) as f32;
            let row = ((slot - 1) / 5) as f32;
            let rect = Rect::from_min_size(
                Pos2::new(
                    grid_left + col * (unit + gap),
                    grid_top + row * (unit + gap),
                ),
                Vec2::splat(unit),
            );
            let hovered = ui
                .rect_contains_pointer(rect)
                .then(|| ui.ctx().pointer_hover_pos())
                .flatten()
                .is_some_and(|p| rect.contains(p));

            clip.rect(
                rect,
                CornerRadius::same(radius),
                Color32::from_rgb(11, 13, 18),
                Stroke::new(1.0, Color32::from_white_alpha(10)),
                StrokeKind::Inside,
            );

            let image = rect.shrink(0.035 * unit);
            paint_tile(&clip, self.tiles.key[slot as usize - 1].as_ref(), image);

            let selected = self.selected == Some(Selection::Key(slot));
            if hovered || selected {
                let color = if selected {
                    ACCENT
                } else {
                    Color32::from_rgba_unmultiplied(74, 158, 255, 170)
                };
                if selected {
                    clip.rect(
                        rect.expand(3.0),
                        CornerRadius::same(radius.saturating_add(3)),
                        Color32::TRANSPARENT,
                        Stroke::new(4.0, Color32::from_rgba_unmultiplied(74, 158, 255, 28)),
                        StrokeKind::Outside,
                    );
                }
                clip.rect(
                    rect.expand(0.5),
                    CornerRadius::same(radius.saturating_add(1)),
                    Color32::TRANSPARENT,
                    Stroke::new(if selected { 2.5 } else { 1.8 }, color),
                    StrokeKind::Outside,
                );
                if hovered {
                    let c = Pos2::new(image.right() - 0.09 * unit, image.top() + 0.09 * unit);
                    let r = (0.022 * unit).clamp(1.2, 2.6);
                    for k in 0..3 {
                        clip.circle_filled(
                            Pos2::new(c.x - k as f32 * r * 2.6, c.y),
                            r,
                            ACCENT,
                        );
                    }
                }
            }
            if egui::DragAndDrop::payload::<ComponentPayload>(ui.ctx()).is_some_and(|payload| matches!(payload.as_ref(), ComponentPayload::Key(_))) {
                clip.rect(rect.shrink(1.0), CornerRadius::same(radius), Color32::from_rgba_unmultiplied(74, 158, 255, 34), Stroke::new(2.0, ACCENT), StrokeKind::Inside);
            }
            if egui::DragAndDrop::payload::<CellDrag>(ui.ctx()).is_some_and(|payload| matches!(*payload, CellDrag::Key(from) if from != slot)) {
                let target = rect.shrink(1.0);
                clip.rect(target, CornerRadius::same(radius), if hovered { Color32::from_rgba_unmultiplied(74, 158, 255, 78) } else { Color32::from_rgba_unmultiplied(74, 158, 255, 26) }, Stroke::new(if hovered { 3.0 } else { 1.8 }, ACCENT), StrokeKind::Inside);
            }

            let resp = ui
                .interact(rect, ui.scope_id().with(("deck-key", slot)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);
            if hovered {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }

            let cfg = keys.iter().find(|k| k.index() == slot);
            let hint = match cfg {
                Some(KeyCfg::Metric { metric, .. }) => {
                    format!("Клавиша {slot} · {} — нажмите, чтобы настроить", metric.label())
                }
                Some(KeyCfg::NextPage { .. }) => {
                    format!("Клавиша {slot} · переход на страницу — нажмите, чтобы настроить")
                }
                Some(KeyCfg::Launch { path, .. }) => {
                    format!("Клавиша {slot} · запуск {path} — нажмите, чтобы настроить")
                }
                Some(KeyCfg::Hotkey { key, .. }) => {
                    format!("Клавиша {slot} · сочетание {key} — нажмите, чтобы настроить")
                }
                Some(KeyCfg::Media { action, .. }) => {
                    format!("Клавиша {slot} · {} — нажмите, чтобы настроить", action.label())
                }
                None => format!("Клавиша {slot} · пусто — нажмите, чтобы назначить"),
            };
            let resp = resp.on_hover_text(hint);

            if cfg.is_some() {
                let handle_rect = Rect::from_min_size(Pos2::new(rect.right() - 23.0, rect.top() + 2.0), Vec2::splat(20.0));
                ui.put(handle_rect, |ui: &mut egui::Ui| {
                    ui.dnd_drag_source(ui.scope_id().with(("key-cell-drag", slot)), CellDrag::Key(slot), |ui| {
                        grip_handle(ui)
                    }).response.on_hover_text("Перетащить ячейку")
                });
            }

            if resp.clicked() {
                if cfg.is_none() { self.library_tab = LibraryTab::Button; self.selected = Some(Selection::Library); self.inspector_open = true; }
                else { self.open_cell(Selection::Key(slot)); }
            }
            if egui::DragAndDrop::payload::<ComponentPayload>(ui.ctx()).is_some() {
            if let Some(payload) = resp.dnd_release_payload::<ComponentPayload>() {
                if let ComponentPayload::Key(template) = payload.as_ref().clone() {
                    let cfg = match template {
                        KeyTemplate::Metric(metric) => KeyCfg::Metric {
                            index: slot,
                            metric,
                            style: Style::Value,
                            interval_ms: None,
                            battery_device: None,
                            on_press: crate::config::TileAction::NoAction,
                            colors: TileColors::default(),
                        },
                        KeyTemplate::NextPage => KeyCfg::NextPage { index: slot, colors: TileColors::default() },
                        KeyTemplate::Launch => KeyCfg::Launch { index: slot, path: String::new(), colors: TileColors::default() },
                        KeyTemplate::Hotkey => KeyCfg::Hotkey { index: slot, key: "A".into(), ctrl: false, alt: false, shift: false, win: false, colors: TileColors::default() },
                        KeyTemplate::Media => KeyCfg::Media { index: slot, action: MediaAction::PlayPause, colors: TileColors::default() },
                    };
                    if let Some(page) = self.page_mut() { page.keys.retain(|k| k.index() != slot); page.keys.push(cfg); page.keys.sort_by_key(|k| k.index()); }
                    self.dirty = true; self.tex_dirty = true; self.selected = Some(Selection::Key(slot));
                }
            }
            }
            if egui::DragAndDrop::payload::<CellDrag>(ui.ctx()).is_some() {
            if let Some(payload) = resp.dnd_release_payload::<CellDrag>() {
                self.cell_drop_handled = true;
                if let CellDrag::Key(from) = *payload {
                    if from != slot {
                        if let Some(page) = self.page_mut() {
                            let a = page.keys.iter().position(|k| k.index() == from);
                            let b = page.keys.iter().position(|k| k.index() == slot);
                            match (a, b) {
                                (Some(a), Some(b)) => { page.keys.swap(a, b); page.keys[a].set_index(from); page.keys[b].set_index(slot); }
                                (Some(a), None) => page.keys[a].set_index(slot),
                                _ => {}
                            }
                        }
                        self.dirty = true; self.tex_dirty = true;
                    }
                }
            }
            }
        }

        // ---- полоса тач-стрипа ----
        let zone_w = unit * ZONE_W as f32 / KEY_W as f32;
        let strip_w = zone_w * 4.0;
        let strip_left = body.center().x - strip_w / 2.0;
        let strip_top = grid_top + 2.0 * unit + gap + 0.27 * unit;
        let strip_rect = Rect::from_min_size(
            Pos2::new(strip_left, strip_top),
            Vec2::new(strip_w, unit),
        );

        clip.rect(
            strip_rect.expand(0.055 * unit),
            CornerRadius::same((0.1 * unit) as u8),
            Color32::from_rgb(8, 9, 13),
            Stroke::new(1.0, Color32::from_white_alpha(12)),
            StrokeKind::Inside,
        );

        // Поля слева/справа от панели сенсоров — настройка свайпов.
        // При наведении подсвечивается вся панель целиком.
        let margin = (0.22 * unit).max(18.0);
        let left_margin = Rect::from_min_max(
            Pos2::new(strip_rect.left() - margin, strip_rect.top()),
            Pos2::new(strip_rect.left(), strip_rect.bottom()),
        );
        let right_margin = Rect::from_min_max(
            Pos2::new(strip_rect.right(), strip_rect.top()),
            Pos2::new(strip_rect.right() + margin, strip_rect.bottom()),
        );
        let strip_selected = self.selected == Some(Selection::StripSwipe);
        let strip_hovered = ui.ctx().pointer_hover_pos().is_some_and(|p| {
            left_margin.contains(p) || right_margin.contains(p)
        });
        if strip_selected || strip_hovered {
            clip.rect(
                strip_rect.expand(0.04 * unit),
                CornerRadius::same((0.1 * unit) as u8),
                if strip_selected {
                    Color32::from_rgba_unmultiplied(74, 158, 255, 28)
                } else {
                    Color32::from_rgba_unmultiplied(74, 158, 255, 14)
                },
                Stroke::new(
                    if strip_selected { 2.5 } else { 1.6 },
                    if strip_selected { ACCENT } else { Color32::from_rgba_unmultiplied(74, 158, 255, 160) },
                ),
                StrokeKind::Outside,
            );
        }
        for (side, rect) in [("left", left_margin), ("right", right_margin)] {
            let resp = ui
                .interact(rect, ui.scope_id().with(("strip-swipe", side)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text("Свайпы панели · нажмите, чтобы настроить");
            if resp.clicked() {
                self.open_cell(Selection::StripSwipe);
            }
        }

        for slot in 1..=4u8 {
            let rect = Rect::from_min_size(
                Pos2::new(strip_left + (slot - 1) as f32 * zone_w, strip_top),
                Vec2::new(zone_w, unit),
            );
            paint_tile(&clip, self.tiles.zone[slot as usize - 1].as_ref(), rect);

            let selected = self.selected == Some(Selection::Zone(slot));
            let hovered = ui.ctx().pointer_hover_pos().is_some_and(|p| rect.contains(p));

            if selected || hovered {
                if selected {
                    clip.rect(
                        rect.shrink(1.0),
                        CornerRadius::same(1),
                        Color32::TRANSPARENT,
                        Stroke::new(2.5, ACCENT),
                        StrokeKind::Inside,
                    );
                } else {
                    clip.rect(
                        rect,
                        CornerRadius::same(1),
                        Color32::from_white_alpha(8),
                        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 60)),
                        StrokeKind::Inside,
                    );
                }
            }
            if egui::DragAndDrop::payload::<ComponentPayload>(ui.ctx()).is_some_and(|payload| matches!(payload.as_ref(), ComponentPayload::SensorAppVolume | ComponentPayload::SensorMetric(_))) {
                clip.rect(rect.shrink(1.0), CornerRadius::same(2), Color32::from_rgba_unmultiplied(53, 208, 127, 35), Stroke::new(2.0, GREEN), StrokeKind::Inside);
            }
            if egui::DragAndDrop::payload::<CellDrag>(ui.ctx()).is_some_and(|payload| matches!(*payload, CellDrag::Zone(from) if from != slot)) {
                clip.rect(rect.shrink(1.0), CornerRadius::same(2), if hovered { Color32::from_rgba_unmultiplied(53, 208, 127, 82) } else { Color32::from_rgba_unmultiplied(53, 208, 127, 28) }, Stroke::new(if hovered { 3.0 } else { 1.8 }, GREEN), StrokeKind::Inside);
            }

            let zone_cfg = zones.iter().find(|z| z.index == slot);
            let volume = volumes.get(slot as usize - 1).copied().unwrap_or(50);
            let is_muted = muted.get(slot as usize - 1).copied().unwrap_or(false);
            let title = zone_cfg.map(|z| z.title.as_str()).unwrap_or("VOL");

            let hint = if is_muted {
                format!("Зона {slot} · {title} · заглушено — нажмите, чтобы настроить")
            } else {
                format!("Зона {slot} · {title} · {volume}% — нажмите, чтобы настроить")
            };

            let resp = ui
                .interact(rect, ui.scope_id().with(("deck-zone", slot)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text(hint);
            if zone_cfg.is_some() {
                let handle_rect = Rect::from_min_size(Pos2::new(rect.right() - 22.0, rect.top() + 1.0), Vec2::new(20.0, 18.0));
                ui.put(handle_rect, |ui: &mut egui::Ui| {
                    ui.dnd_drag_source(ui.scope_id().with(("zone-cell-drag", slot)), CellDrag::Zone(slot), |ui| {
                        grip_handle(ui)
                    }).response.on_hover_text("Перетащить сенсор")
                });
            }
            if resp.clicked() {
                if zone_cfg.is_none() { self.library_tab = LibraryTab::Sensor; self.selected = Some(Selection::Library); self.inspector_open = true; }
                else { self.open_cell(Selection::Zone(slot)); }
            }
            if egui::DragAndDrop::payload::<ComponentPayload>(ui.ctx()).is_some() {
            if let Some(payload) = resp.dnd_release_payload::<ComponentPayload>() {
                match *payload {
                    ComponentPayload::SensorAppVolume => {
                        self.ensure_zone(slot);
                        if let Some(page) = self.page_mut() {
                            if let Some(zone) = page.zones.iter_mut().find(|z| z.index == slot) {
                                // Замена содержимого слота: старая метрика/заряд снимаются.
                                if zone.metric.take().is_some() {
                                    zone.title = format!("VOL {slot}");
                                    zone.battery_device = None;
                                    zone.style = Style::Value;
                                }
                                zone.dynamic = false;
                            }
                            page.auto_bind_free_encoder(slot);
                        }
                        self.selected = Some(Selection::Zone(slot)); self.dirty = true; self.tex_dirty = true;
                    }
                    ComponentPayload::SensorMetric(metric) => {
                        self.ensure_zone(slot);
                        if let Some(zone) = self.page_mut().and_then(|p| p.zones.iter_mut().find(|z| z.index == slot)) {
                            // Замена: приложение на этом сенсоре снимается.
                            zone.metric = Some(metric);
                            zone.app = None;
                            zone.app_path = None;
                            zone.dynamic = false;
                            if metric != Metric::DeviceBattery {
                                zone.battery_device = None;
                            }
                        }
                        self.selected = Some(Selection::Zone(slot)); self.dirty = true; self.tex_dirty = true;
                    }
                    ComponentPayload::Key(_) => {}
                }
            }
            }
            if egui::DragAndDrop::payload::<CellDrag>(ui.ctx()).is_some() {
            if let Some(payload) = resp.dnd_release_payload::<CellDrag>() {
                self.cell_drop_handled = true;
                if let CellDrag::Zone(from) = *payload {
                    if from != slot {
                        let mut destination_occupied = false;
                        if let Some(page) = self.page_mut() {
                            let a = page.zones.iter().position(|z| z.index == from);
                            let b = page.zones.iter().position(|z| z.index == slot);
                            destination_occupied = b.is_some();
                            match (a, b) {
                                (Some(a), Some(b)) => { page.zones.swap(a, b); page.zones[a].index = from; page.zones[b].index = slot; }
                                (Some(a), None) => page.zones[a].index = slot,
                                _ => {}
                            }
                            page.move_encoder_links(from, slot, destination_occupied);
                        }
                        // Громкости в shared привязаны к слотам сенсоров — двигаем вместе с контентом.
                        if let Ok(mut shared) = self.shared.lock() {
                            let from_i = (from as usize).saturating_sub(1).min(3);
                            let to_i = (slot as usize).saturating_sub(1).min(3);
                            if from_i != to_i {
                                if destination_occupied {
                                    shared.volumes.swap(from_i, to_i);
                                    shared.muted.swap(from_i, to_i);
                                } else {
                                    shared.volumes[to_i] = shared.volumes[from_i];
                                    shared.muted[to_i] = shared.muted[from_i];
                                    shared.volumes[from_i] = 50;
                                    shared.muted[from_i] = false;
                                }
                            }
                        }
                        self.dirty = true; self.tex_dirty = true;
                    }
                }
            }
            }
        }

        // ---- крутилки ----
        let plate_top = strip_rect.bottom() + 0.17 * unit;
        let plate = Rect::from_min_max(
            Pos2::new(strip_left, plate_top),
            Pos2::new(strip_left + strip_w, body.bottom() - 0.15 * unit),
        );
        clip.rect(
            plate,
            CornerRadius::same((0.07 * unit) as u8),
            Color32::from_rgb(29, 33, 43),
            Stroke::new(1.0, Color32::from_white_alpha(6)),
            StrokeKind::Inside,
        );
        // Лёгкая «металлическая» штриховка.
        let mut y = plate.top() + 4.0;
        while y < plate.bottom() {
            clip.line_segment(
                [Pos2::new(plate.left() + 1.0, y), Pos2::new(plate.right() - 1.0, y)],
                Stroke::new(0.5, Color32::from_white_alpha(4)),
            );
            y += 5.0;
        }

        let knob_cy = plate.center().y;
        let knob_r = 0.3 * unit;

        for slot in 1..=4u8 {
            // Крутилка рисуется под сенсором, к которому привязана. Без привязки
            // остаётся на своём «домашнем» месте. Не показываем призрак ушедшей крутилки.
            let encoder_id = encoders.iter().find(|e| e.sensor_index == Some(slot)).map(|e| e.index)
                .or_else(|| encoders.iter().find(|e| e.index == slot && e.sensor_index.is_none()).map(|e| e.index));
            let Some(encoder_id) = encoder_id else { continue };
            let center = Pos2::new(strip_left + (slot as f32 - 0.5) * zone_w, knob_cy);
            let volume = volumes.get(slot as usize - 1).copied().unwrap_or(50).clamp(0, 100);
            let is_muted = muted.get(slot as usize - 1).copied().unwrap_or(false);
            let selected = self.selected == Some(Selection::Encoder(encoder_id));
            let hovered = ui
                .ctx()
                .pointer_hover_pos()
                .is_some_and(|p| (p - center).length() <= knob_r * 1.15);

            // Дуга уровня по ободу.
            arc(
                &clip,
                center,
                knob_r * 1.02,
                135.0,
                405.0,
                3.0,
                Color32::from_white_alpha(18),
            );
            if volume > 0 {
                let color = if is_muted { AMBER } else { ACCENT };
                arc(
                    &clip,
                    center,
                    knob_r * 1.02,
                    135.0,
                    135.0 + 270.0 * volume as f32 / 100.0,
                    3.0,
                    color,
                );
            }

            // Корпус крутилки.
            clip.circle_filled(center, knob_r, Color32::from_rgb(18, 21, 28));
            clip.circle_filled(center, knob_r * 0.94, Color32::from_rgb(45, 52, 66));
            clip.circle_filled(
                center + Vec2::new(0.0, -knob_r * 0.05),
                knob_r * 0.82,
                Color32::from_rgb(52, 60, 75),
            );
            clip.circle_stroke(center, knob_r * 0.82, Stroke::new(1.0, Color32::from_white_alpha(14)));
            clip.circle_filled(
                center + Vec2::new(-knob_r * 0.18, -knob_r * 0.22),
                knob_r * 0.42,
                Color32::from_rgba_unmultiplied(255, 255, 255, 12),
            );

            // Риска положения.
            let angle = (135.0 + 270.0 * volume as f32 / 100.0).to_radians();
            let inner = center + Vec2::new(angle.cos(), angle.sin()) * knob_r * 0.35;
            let outer_pt = center + Vec2::new(angle.cos(), angle.sin()) * knob_r * 0.74;
            clip.line_segment(
                [inner, outer_pt],
                Stroke::new((0.035 * unit).max(2.0), Color32::from_rgb(28, 33, 43)),
            );

            clip.text(
                center + Vec2::new(0.0, knob_r * 0.02),
                Align2::CENTER_CENTER,
                if is_muted { "MUTE".to_string() } else { format!("{volume}%") },
                FontId::proportional((0.12 * unit).max(8.0)),
                if is_muted { AMBER } else { Color32::from_rgb(190, 200, 216) },
            );

            if selected || hovered {
                clip.circle_stroke(
                    center,
                    knob_r * 1.12,
                    Stroke::new(if selected { 2.0 } else { 1.2 }, ACCENT),
                );
            }

            let resp = ui
                .interact(
                    Rect::from_center_size(center, Vec2::splat(knob_r * 2.4)),
                    ui.scope_id().with(("deck-knob", encoder_id)),
                    Sense::click(),
                )
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text(format!(
                    "Крутилка {encoder_id} · {volume}%{} — нажмите, чтобы настроить",
                    if is_muted { " · заглушено" } else { "" }
                ));
            if resp.clicked() { self.open_cell(Selection::Encoder(encoder_id)); }
        }

        // Винтики в углах корпуса.
        for p in [
            Pos2::new(body.left() + 0.16 * unit, body.bottom() - 0.16 * unit),
            Pos2::new(body.right() - 0.16 * unit, body.bottom() - 0.16 * unit),
        ] {
            clip.circle_filled(p, (0.028 * unit).max(1.5), Color32::from_white_alpha(10));
        }
    }

    // -- выдвижная панель --------------------------------------------------

    fn inspector(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        volumes: [i32; 4],
        muted: [bool; 4],
        audio: &AudioState,
        status: &str,
    ) {
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 34.0), Layout::left_to_right(Align::Center), |ui| ui.horizontal(|ui| {
            if ui.selectable_label(self.library_tab == LibraryTab::Button, "Кнопка").clicked() { self.library_tab = LibraryTab::Button; self.selected = Some(Selection::Library); }
            if ui.selectable_label(self.library_tab == LibraryTab::Sensor, "Сенсор").clicked() { self.library_tab = LibraryTab::Sensor; self.selected = Some(Selection::Library); }
        }));
        ui.separator();
        let selection = self.selected.unwrap_or(Selection::Device);
        let (title, subtitle, _accent) = match selection {
            Selection::Library => ("Компоненты".to_string(), "перетащите на ячейку устройства".to_string(), ACCENT),
            Selection::Key(index) => {
                let (title, subtitle) = match self.key_cfg(index) {
                    Some(KeyCfg::Metric { metric, style, .. }) => {
                        (metric.label().to_string(), format!("{} · {}", metric.label(), style.label()))
                    }
                    Some(KeyCfg::NextPage { .. }) => ("Следующая страница".into(), "Переход между страницами".into()),
                    Some(KeyCfg::Launch { path, .. }) => ("Приложение или ссылка".into(), format!("{path}")),
                    Some(KeyCfg::Hotkey { key, .. }) => ("Сочетание клавиш".into(), format!("{key}")),
                    Some(KeyCfg::Media { action, .. }) => ("Мультимедиа".into(), action.label().to_string()),
                    None => ("Компоненты кнопок".into(), format!("Клавиша {index}")),
                };
                (title, if self.key_cfg(index).is_some() { format!("Клавиша {index} · {subtitle}") } else { subtitle }, ACCENT)
            }
            Selection::Zone(index) => {
                let title = self
                    .page()
                    .and_then(|p| p.zones.iter().find(|z| z.index == index))
                    .map(|z| z.title.clone())
                    .unwrap_or_else(|| format!("VOL {index}"));
                (
                    format!("Зона {index}"),
                    format!("тач-стрип · {title}"),
                    GREEN,
                )
            }
            Selection::Encoder(index) => ("Крутилка".to_string(), format!("Энкодер {index}"), GREEN),
            Selection::StripSwipe => (
                "Свайпы панели".to_string(),
                "тач-стрип · влево / вправо".to_string(),
                ACCENT,
            ),
            Selection::Device => (
                "Устройство".to_string(),
                "AKP05E · общие настройки".to_string(),
                MUTED,
            ),
        };

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(title).size(16.0).strong().color(TEXT));
                ui.label(egui::RichText::new(subtitle).size(11.0).color(MUTED));
            });
        });

        ui.add_space(10.0);
        ui.separator();

        let body_height = ui.available_height().max(120.0);
        egui::ScrollArea::vertical()
            .id_salt("inspector_body")
            .max_height(body_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The inspector has a fixed width. An unbounded slider here can
                // claim the center panel's space and make the layout collapse.
                ui.spacing_mut().slider_width = ui.available_width().min(336.0).max(100.0);
                match selection {
                    Selection::Library => self.library_panel(ui, audio),
                    Selection::Key(index) => self.key_inspector(ui, index, snapshot),
                    Selection::Zone(index) => self.zone_inspector(ui, index, volumes, muted, audio, snapshot),
                    Selection::Encoder(index) => self.encoder_inspector(ui, index),
                    Selection::StripSwipe => self.strip_swipe_inspector(ui),
                    Selection::Device => self.device_inspector(ui),
                }
                ui.add_space(10.0);
            });

    }

    fn library_panel(&mut self, ui: &mut egui::Ui, _audio: &AudioState) {
        match self.library_tab {
            LibraryTab::Button => {
                ui.label(egui::RichText::new("Компоненты кнопок").size(14.0).strong());
                egui::CollapsingHeader::new("Команды и действия").default_open(true).show(ui, |ui| {
                    for (name, payload) in [
                    ("Следующая страница", KeyTemplate::NextPage),
                    ("Приложение или ссылка", KeyTemplate::Launch),
                    ("Сочетание клавиш", KeyTemplate::Hotkey),
                    ("Мультимедиа", KeyTemplate::Media),
                    ] {
                        ui.dnd_drag_source(ui.scope_id().with(("component", name)), ComponentPayload::Key(payload), |ui| {
                            component_row(ui, name, IconKind::Action);
                        });
                    }
                });
                ui.add_space(6.0);
                egui::CollapsingHeader::new("Мониторинг").default_open(true).show(ui, |ui| {
                for (group, metrics) in metric_groups() {
                    egui::CollapsingHeader::new(group).default_open(group == "Процессор").show(ui, |ui| {
                        for metric in metrics {
                            let icon = metric_icon(*metric);
                            ui.dnd_drag_source(ui.scope_id().with(("metric", group, metric.label())), ComponentPayload::Key(KeyTemplate::Metric(*metric)), |ui| {
                                component_row(ui, metric.label(), icon)
                            });
                        }
                    });
                }
                });
            }
            LibraryTab::Sensor => {
                ui.label(egui::RichText::new("Компоненты сенсора").size(14.0).strong());
                let has_audio = self.shared.lock().ok().and_then(|shared| shared.audio.lock().ok().map(|audio| !audio.sessions.is_empty())).unwrap_or(false);
                if has_audio {
                    ui.dnd_drag_source(ui.scope_id().with("sensor-app-volume"), ComponentPayload::SensorAppVolume, |ui| {
                        component_row(ui, "Громкость приложения", IconKind::Audio)
                    });
                }
                egui::CollapsingHeader::new("Мониторинг").default_open(true).show(ui, |ui| {
                    let snapshot = self.shared.lock().ok().and_then(|shared| shared.snapshot.clone()).unwrap_or_default();
                    for (group, metrics) in metric_groups() {
                        let available = metrics.iter().copied().filter(|metric| metric_available(*metric, &snapshot)).collect::<Vec<_>>();
                        if available.is_empty() { continue; }
                        egui::CollapsingHeader::new(group).show(ui, |ui| for metric in &available {
                            let kind = metric_icon(*metric);
                            ui.dnd_drag_source(ui.scope_id().with(("sensor-metric", group, metric.label())), ComponentPayload::SensorMetric(*metric), |ui| component_row(ui, metric.label(), kind));
                        });
                    }
                });
            }
        }
    }

    fn key_inspector(&mut self, ui: &mut egui::Ui, index: u8, snapshot: &Snapshot) {
        let Some(cfg) = self.key_cfg(index).cloned() else {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Выберите компонент справа и перетащите его на кнопку.")
                    .size(12.0)
                    .color(MUTED),
            );
            return;
        };

        if let KeyCfg::NextPage { .. } = cfg {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Плитка перехода на следующую страницу")
                    .size(13.0)
                    .color(TEXT),
            );
            ui.label(
                egui::RichText::new(
                    if self.pages().len() > 1 {
                        "Нажатие переключит на следующую страницу."
                    } else {
                        "Добавьте ещё одну страницу, чтобы переключение было доступно."
                    },
                )
                .size(11.0)
                .color(MUTED),
            );
            return;
        }

        if let KeyCfg::Launch { path, .. } = cfg {
            section_label(ui, "ПРИЛОЖЕНИЕ ИЛИ ССЫЛКА");
            ui.add_space(5.0);
            let mut edited_path = path.clone();
            let response = ui.add(
                egui::TextEdit::singleline(&mut edited_path)
                    .desired_width(f32::INFINITY)
                    .hint_text("C:\\Путь\\к\\программе.exe или https://…"),
            );
            if response.changed() {
                if let Some(KeyCfg::Launch { path, .. }) = self.key_cfg_mut(index) {
                    *path = edited_path;
                }
                self.dirty = true;
                self.tex_dirty = true;
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Команда запускается по нажатию кнопки через обработчик Windows.")
                    .size(11.0)
                    .color(MUTED),
            );
            if path.trim().is_empty() {
                ui.label(
                    egui::RichText::new("Заполни поле перед применением конфигурации.")
                        .size(11.0)
                        .color(RED),
                );
            }
            return;
        }

        if let KeyCfg::Hotkey { key, ctrl, alt, shift, win, .. } = cfg {
            section_label(ui, "СОЧЕТАНИЕ КЛАВИШ");
            ui.add_space(5.0);
            let mut next_key = key.clone();
            egui::ComboBox::from_id_salt(("hotkey-key", index))
                .selected_text(&next_key)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for key in hotkey_choices() {
                        ui.selectable_value(&mut next_key, key.to_string(), key);
                    }
                });
            let mut next_ctrl = ctrl;
            let mut next_alt = alt;
            let mut next_shift = shift;
            let mut next_win = win;
            ui.horizontal(|ui| {
                ui.checkbox(&mut next_ctrl, "Ctrl");
                ui.checkbox(&mut next_alt, "Alt");
                ui.checkbox(&mut next_shift, "Shift");
                ui.checkbox(&mut next_win, "Win");
            });
            if next_key.as_str() != key.as_str()
                || next_ctrl != ctrl
                || next_alt != alt
                || next_shift != shift
                || next_win != win
            {
                if let Some(KeyCfg::Hotkey { key, ctrl, alt, shift, win, .. }) = self.key_cfg_mut(index) {
                    *key = next_key;
                    *ctrl = next_ctrl;
                    *alt = next_alt;
                    *shift = next_shift;
                    *win = next_win;
                }
                self.dirty = true;
                self.tex_dirty = true;
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Отправка выполняется при нажатии кнопки. Некоторые игры могут блокировать SendInput.")
                    .size(11.0)
                    .color(MUTED),
            );
            return;
        }

        if let KeyCfg::Media { action, .. } = cfg {
            section_label(ui, "МУЛЬТИМЕДИА");
            ui.add_space(5.0);
            let mut next_action = action;
            egui::ComboBox::from_id_salt(("media-action", index))
                .selected_text(action.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for item in MediaAction::ALL {
                        ui.selectable_value(&mut next_action, item, item.label());
                    }
                });
            if next_action != action {
                if let Some(KeyCfg::Media { action, .. }) = self.key_cfg_mut(index) {
                    *action = next_action;
                }
                self.dirty = true;
                self.tex_dirty = true;
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Событие мультимедиа отправляется только при нажатии кнопки.")
                    .size(11.0)
                    .color(MUTED),
            );
            return;
        }

        let (mut metric, mut style, mut interval) = match cfg {
            KeyCfg::Metric {
                metric,
                style,
                interval_ms,
                ..
            } => (metric, style, interval_ms),
            _ => return,
        };

        // Метрика.
        setting_title(ui, "Параметр", "Выберите системную метрику для этой кнопки.");
        ui.add_space(4.0);
        let mut next_metric = metric;
        ui.columns(2, |cols| {
            let (left, right) = Metric::ALL.split_at(Metric::ALL.len() / 2);
            for (col, items) in cols.iter_mut().zip([left, right]) {
                for m in items {
                    let selected = *m == metric;
                    let text = egui::RichText::new(m.label())
                        .size(11.5)
                        .color(if selected { ACCENT_TEXT } else { TEXT });
                    let button = egui::Button::new(text)
                        .selected(selected)
                        .fill(if selected { ACCENT_DARK } else { CARD_2 })
                        .corner_radius(8.0);
                    if col
                        .add_sized([col.available_width(), 30.0], button)
                        .clicked()
                    {
                        next_metric = *m;
                    }
                    col.add_space(-2.0);
                }
            }
        });
        if next_metric != metric {
            metric = next_metric;
            if let Some(KeyCfg::Metric { metric: m, .. }) = self.key_cfg_mut(index) {
                *m = metric;
            }
            self.dirty = true;
            self.tex_dirty = true;
        }

        let battery_device = match self.key_cfg(index) {
            Some(KeyCfg::Metric { battery_device, .. }) => battery_device.clone(),
            _ => None,
        };

        if metric == Metric::DeviceBattery {
            ui.add_space(14.0);
            setting_title(
                ui,
                "Устройство",
                "Мышь (донгл/USB), DualSense или другое устройство с зарядом.",
            );
            ui.add_space(4.0);
            let devices = &snapshot.batteries;
            let selected_label = battery_device
                .as_deref()
                .and_then(|id| devices.iter().find(|d| d.id == id))
                .map(|d| {
                    let pct = d
                        .percent
                        .map(|p| format!("{p}%"))
                        .unwrap_or_else(|| "—".into());
                    let charge = if d.charging { " ⚡" } else { "" };
                    format!("{} · {pct}{charge}", d.name)
                })
                .unwrap_or_else(|| {
                    if devices.is_empty() {
                        "нет подключённых устройств".into()
                    } else {
                        "выберите устройство".into()
                    }
                });
            let mut next = battery_device.clone();
            egui::ComboBox::from_id_salt(("battery-device", index))
                .selected_text(selected_label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut next, None, "не выбрано");
                    for device in devices {
                        let pct = device
                            .percent
                            .map(|p| format!("{p}%"))
                            .unwrap_or_else(|| "—".into());
                        let charge = if device.charging { " ⚡" } else { "" };
                        let label = format!(
                            "{} · {} · {pct}{charge}",
                            device.kind.label(),
                            device.name
                        );
                        ui.selectable_value(&mut next, Some(device.id.clone()), label);
                    }
                });
            if next != battery_device {
                if let Some(KeyCfg::Metric { battery_device, .. }) = self.key_cfg_mut(index) {
                    *battery_device = next;
                }
                self.dirty = true;
                self.tex_dirty = true;
            }
            if devices.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "Подключите мышь (Aurora/Sinowealth), DualSense или BT-устройство с Battery Service — они появятся в списке.",
                    )
                    .size(11.0)
                    .color(MUTED),
                );
            }
        }

        let battery_device = match self.key_cfg(index) {
            Some(KeyCfg::Metric { battery_device, .. }) => battery_device.clone(),
            _ => None,
        };

        // Оформление (для заряда фиксированный layout — без выбора стиля).
        if metric != Metric::DeviceBattery {
            ui.add_space(14.0);
            setting_title(ui, "Оформление", "Выберите, как значение будет показано на экране устройства.");
            ui.add_space(4.0);
            self.ensure_style_previews(ui.ctx(), metric, snapshot, battery_device.as_deref());
            let mut next_style = style;
            ui.columns(3, |cols| {
                for (i, s) in Style::ALL.iter().enumerate() {
                    let selected = *s == style;
                    if style_card(
                        &mut cols[i],
                        self.style_previews[i].as_ref(),
                        s.label(),
                        selected,
                    ) {
                        next_style = *s;
                    }
                }
            });
            if next_style != style {
                style = next_style;
                if let Some(KeyCfg::Metric { style: st, .. }) = self.key_cfg_mut(index) {
                    *st = style;
                }
                self.dirty = true;
                self.tex_dirty = true;
            }
        }

        ui.add_space(14.0);

        // Интервал.
        setting_title(ui, "Обновление", "Задайте индивидуальную частоту обновления этой плитки или используйте общий интервал.");
        ui.add_space(4.0);
        let mut custom = interval.is_some();
        ui.horizontal_wrapped(|ui| {
        if ui.checkbox(&mut custom, "свой интервал").changed() {
            interval = if custom { Some(1000) } else { None };
            if let Some(KeyCfg::Metric { interval_ms, .. }) = self.key_cfg_mut(index) {
                *interval_ms = interval;
            }
            self.dirty = true;
            self.tex_dirty = true;
        }
        });
        match &mut interval {
            Some(ms) => {
                ui.add_space(4.0);
                if slider_row_u64(ui, ms, 250..=10_000, " мс", true) {
                    if let Some(KeyCfg::Metric { interval_ms, .. }) = self.key_cfg_mut(index) {
                        *interval_ms = Some(*ms);
                    }
                    self.dirty = true;
                    self.tex_dirty = true;
                }
            }
            None => {
                let common = self.draft.monitor.interval_ms;
                // Та же высота, что у строки слайдера, чтобы блоки ниже не прыгали.
                ui.add_space(4.0);
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 30.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_height(30.0);
                    ui.label(
                        egui::RichText::new(format!("Как у всех: {common} мс"))
                            .size(11.0)
                            .color(FAINT),
                    );
                });
            }
        }

        self.tile_colors_editor(ui, true, metric);

        ui.add_space(14.0);
        setting_title(
            ui,
            "При нажатии",
            "Действие при нажатии на плитку мониторинга: треки, пауза, URL, диспетчер задач, горячая клавиша.",
        );
        ui.add_space(4.0);
        let mut on_press = match self.key_cfg(index) {
            Some(KeyCfg::Metric { on_press, .. }) => on_press.clone(),
            _ => TileAction::NoAction,
        };
        if self.tile_action_editor(ui, egui::Id::new(("metric-press", index)), &mut on_press) {
            if let Some(KeyCfg::Metric { on_press: slot, .. }) = self.key_cfg_mut(index) {
                *slot = on_press;
            }
            self.dirty = true;
        }

        ui.add_space(12.0);
        let width = ui.available_width();
        ui.add_space(4.0);
        if ui
            .add_sized(
                [width, 32.0],
                egui::Button::new(egui::RichText::new("Сбросить ячейку").size(11.5).color(Color32::WHITE)).fill(RED),
            )
            .clicked()
        {
            self.confirm_delete = Some(DeleteTarget::Key(index));
        }
    }

    fn zone_inspector(
        &mut self,
        ui: &mut egui::Ui,
        index: u8,
        volumes: [i32; 4],
        muted: [bool; 4],
        audio: &AudioState,
        snapshot: &Snapshot,
    ) {
        let position = self
            .page()
            .and_then(|p| p.zones.iter().position(|z| z.index == index));
        let Some(position) = position else {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Эта зона ещё не назначена.")
                    .size(12.0)
                    .color(MUTED),
            );
            ui.add_space(8.0);
            if ui.button("Добавить зону").clicked() {
                self.ensure_zone(index);
            }
            return;
        };

        let is_metric = self.page().and_then(|p| p.zones.get(position)).is_some_and(|z| z.metric.is_some());
        if is_metric {
            section_label(ui, "КОМПОНЕНТ МОНИТОРИНГА");
            let metric = self.page().and_then(|p| p.zones.get(position)).and_then(|z| z.metric).unwrap_or(Metric::CpuLoad);
            let mut style = self.page().and_then(|p| p.zones.get(position)).map(|z| z.style).unwrap_or(Style::Value);
            ui.label(egui::RichText::new(metric.label()).size(12.0).color(MUTED));
            if metric == Metric::DeviceBattery {
                ui.add_space(8.0);
                setting_title(ui, "Устройство", "Какой заряд показывать на этом сенсоре.");
                let current = self
                    .page()
                    .and_then(|p| p.zones.get(position))
                    .and_then(|z| z.battery_device.clone());
                let devices = &snapshot.batteries;
                let mut next = current.clone();
                let label = current
                    .as_deref()
                    .and_then(|id| devices.iter().find(|d| d.id == id))
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "выберите устройство".into());
                egui::ComboBox::from_id_salt(("zone-battery", index))
                    .selected_text(label)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut next, None, "не выбрано");
                        for device in devices {
                            ui.selectable_value(&mut next, Some(device.id.clone()), &device.name);
                        }
                    });
                if next != current {
                    if let Some(zone) = self.page_mut().and_then(|p| p.zones.get_mut(position)) {
                        zone.battery_device = next;
                    }
                    self.dirty = true;
                    self.tex_dirty = true;
                }
            }

            let battery_device = self
                .page()
                .and_then(|p| p.zones.get(position))
                .and_then(|z| z.battery_device.clone());

            if metric != Metric::DeviceBattery {
                ui.add_space(14.0);
                setting_title(ui, "Оформление", "Как значение будет показано на сенсоре.");
                ui.add_space(4.0);
                self.ensure_style_previews(ui.ctx(), metric, snapshot, battery_device.as_deref());
                let mut next_style = style;
                ui.columns(3, |cols| {
                    for (i, s) in Style::ALL.iter().enumerate() {
                        let selected = *s == style;
                        if style_card(
                            &mut cols[i],
                            self.style_previews[i].as_ref(),
                            s.label(),
                            selected,
                        ) {
                            next_style = *s;
                        }
                    }
                });
                if next_style != style {
                    style = next_style;
                    if let Some(zone) = self.page_mut().and_then(|p| p.zones.get_mut(position)) {
                        zone.style = style;
                    }
                    self.dirty = true;
                    self.tex_dirty = true;
                }
            }

            self.tile_colors_editor(ui, true, metric);
        } else {
        // Приложение.
        setting_title(ui, "Приложение Windows", "Выберите аудиосессию Windows, громкость которой показывает сенсор. Один микшер нельзя назначить двум сенсорам.");
        ui.add_space(4.0);

        let current_app = self
            .page()
            .and_then(|p| p.zones.get(position))
            .and_then(|z| z.app.clone());
        let current_path = self
            .page()
            .and_then(|p| p.zones.get(position))
            .and_then(|z| z.app_path.clone());
        let is_dynamic = self
            .page()
            .and_then(|p| p.zones.get(position))
            .is_some_and(|z| z.dynamic);
        let used_apps = self.page().map(|p| p.used_apps(Some(index))).unwrap_or_default();

        let selected_label = if is_dynamic {
            current_app
                .as_deref()
                .map(|app| match audio.session(app) {
                    Some(session) => format!("Динамический · {} · {}%", session.name, session.volume),
                    None => format!("Динамический · ищет…"),
                })
                .unwrap_or_else(|| "Динамический захват".to_string())
        } else {
            current_app
                .as_deref()
                .map(|app| match audio.session(app) {
                    Some(session) => format!("{} · {}%", session.name, session.volume),
                    None => format!("{app} · нет сессии"),
                })
                .unwrap_or_else(|| "не назначено".to_string())
        };

        #[derive(Clone, PartialEq)]
        enum AppChoice { None, Dynamic, App(String) }
        let mut choice = if is_dynamic {
            AppChoice::Dynamic
        } else if let Some(app) = current_app.clone() {
            AppChoice::App(app)
        } else {
            AppChoice::None
        };
        let previous = choice.clone();

        egui::ComboBox::from_id_salt(("zone-app", index))
            .width(ui.available_width())
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut choice, AppChoice::None, "не назначено");
                ui.selectable_value(&mut choice, AppChoice::Dynamic, "Динамический захват");

                let mut seen = HashSet::new();
                for session in &audio.sessions {
                    if self.draft.audio.is_hidden(&session.app) {
                        continue;
                    }
                    if !seen.insert(session.app.clone()) {
                        continue;
                    }
                    if used_apps.contains(&session.app) && current_app.as_ref() != Some(&session.app) {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        if let Some(icon) = self.audio_icon_textures.get(&session.app) {
                            ui.add(egui::Image::new((icon.id(), Vec2::splat(20.0))));
                        }
                        ui.selectable_value(&mut choice, AppChoice::App(session.app.clone()), format!("{} · {}%", session.name, session.volume));
                    });
                }

                if let Some(app) = &current_app {
                    if !seen.contains(app) && !is_dynamic {
                        ui.selectable_value(
                            &mut choice,
                            AppChoice::App(app.clone()),
                            format!("{app} · нет сессии"),
                        );
                    }
                }
            });

        if choice != previous {
            match choice {
                AppChoice::None => {
                    if let Some(zone) = self.page_mut().and_then(|p| p.zones.get_mut(position)) {
                        zone.dynamic = false;
                        zone.app = None;
                        zone.app_path = None;
                        if zone.title.starts_with("VOL ") || zone.title == "Динамический" || zone.title.is_empty() {
                            zone.title = format!("VOL {index}");
                        }
                    }
                }
                AppChoice::Dynamic => {
                    if let Some(zone) = self.page_mut().and_then(|p| p.zones.get_mut(position)) {
                        zone.dynamic = true;
                        zone.app = None;
                        zone.app_path = None;
                        zone.title = "Динамический".into();
                    }
                    self.last_dynamic_at = Instant::now() - Duration::from_secs(5);
                }
                AppChoice::App(app) => {
                    let new_path = audio.session(&app).map(|session| session.exe_path.clone()).or_else(|| {
                        if current_app.as_ref() == Some(&app) { current_path.clone() } else { None }
                    });
                    let title = audio.session(&app).map(|session| session.name.clone()).unwrap_or_else(|| app_display(&app));
                    if let Some(zone) = self.page_mut().and_then(|p| p.zones.get_mut(position)) {
                        zone.dynamic = false;
                        if zone.title.starts_with("VOL ") || zone.title == "Динамический" || zone.title.is_empty() {
                            zone.title = title;
                        }
                        zone.app = Some(app);
                        zone.app_path = new_path;
                    }
                }
            }
            self.dirty = true;
            self.tex_dirty = true;
        }
        self.tile_colors_editor(ui, false, Metric::CpuLoad);
        }

        ui.add_space(12.0);

        // Название.
        setting_title(ui, "Название сенсора", "Короткая подпись сверху сенсорной плитки. Пустое название оставляет больше места содержимому.");
        ui.add_space(4.0);
        let mut title = self
            .page()
            .map(|p| p.zones[position].title.clone())
            .unwrap_or_default();
        let resp = ui.add(
            egui::TextEdit::singleline(&mut title)
                .desired_width(f32::INFINITY)
                .char_limit(24)
                .hint_text("VOL 1"),
        );
        if resp.changed() {
            if let Some(page) = self.page_mut() {
                if let Some(zone) = page.zones.get_mut(position) {
                    zone.title = title;
                }
            }
            self.dirty = true;
            self.tex_dirty = true;
        }

        ui.add_space(14.0);

        // Громкость.
        section_label(ui, "ГРОМКОСТЬ");
        ui.add_space(4.0);
        let volume = volumes.get(index as usize - 1).copied().unwrap_or(50).clamp(0, 100);
        let is_muted = muted.get(index as usize - 1).copied().unwrap_or(false);

        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{volume}%"))
                            .size(26.0)
                            .strong()
                            .color(if is_muted { AMBER } else { ACCENT }),
                    );
                    if is_muted {
                        pill(ui, "заглушено", AMBER);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("зона {index}"))
                                .size(10.5)
                                .color(FAINT),
                        );
                    });
                });
                ui.add_space(6.0);
                meter(ui, volume as f32 / 100.0, if is_muted { AMBER } else { ACCENT }, 6.0);
            });

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Громкость меняется крутилкой на устройстве, нажатие на крутилку — mute. \
                 Значение на плите обновляется сразу.",
            )
            .size(11.0)
            .color(FAINT),
        );

        ui.add_space(14.0);
        setting_title(
            ui,
            "Нажатие на сенсор",
            "Действие при тапе по этой зоне стрипа: треки, горячая клавиша, страница, URL.",
        );
        ui.add_space(4.0);
        let mut tap_action = self
            .page()
            .and_then(|p| p.zones.get(position))
            .map(|z| z.tap_action.clone())
            .unwrap_or_default();
        if self.tile_action_editor(ui, egui::Id::new(("zone-tap", index)), &mut tap_action) {
            if let Some(zone) = self.page_mut().and_then(|p| p.zones.get_mut(position)) {
                zone.tap_action = tap_action;
            }
            self.dirty = true;
        }

        ui.add_space(16.0);
        let width = ui.available_width();
        if ui
            .add_sized(
                [width, 32.0],
                egui::Button::new(
                    egui::RichText::new("Вернуть название по умолчанию")
                        .size(11.5)
                        .color(MUTED),
                ),
            )
            .clicked()
        {
            if let Some(page) = self.page_mut() {
                if let Some(zone) = page.zones.get_mut(position) {
                    zone.title = format!("VOL {index}");
                }
            }
            self.dirty = true;
            self.tex_dirty = true;
        }

        ui.add_space(8.0);
        if ui.add_sized([ui.available_width(), 32.0], egui::Button::new(egui::RichText::new("Сбросить ячейку").size(11.5).color(Color32::WHITE)).fill(RED)).clicked() {
            self.confirm_delete = Some(DeleteTarget::Zone(index));
        }
    }

    fn strip_swipe_inspector(&mut self, ui: &mut egui::Ui) {
        setting_title(
            ui,
            "Свайп влево",
            "Жест пальцем влево по сенсорной панели. По умолчанию — предыдущая страница.",
        );
        ui.add_space(4.0);
        let mut left = self.page().map(|p| p.swipe_left.clone()).unwrap_or(TileAction::PrevPage);
        if self.tile_action_editor(ui, egui::Id::new("swipe-left"), &mut left) {
            if let Some(page) = self.page_mut() {
                page.swipe_left = left;
            }
            self.dirty = true;
        }

        ui.add_space(16.0);
        setting_title(
            ui,
            "Свайп вправо",
            "Жест пальцем вправо по сенсорной панели. По умолчанию — следующая страница.",
        );
        ui.add_space(4.0);
        let mut right = self.page().map(|p| p.swipe_right.clone()).unwrap_or(TileAction::NextPage);
        if self.tile_action_editor(ui, egui::Id::new("swipe-right"), &mut right) {
            if let Some(page) = self.page_mut() {
                page.swipe_right = right;
            }
            self.dirty = true;
        }

        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(
                "Наведите курсор слева или справа от панели сенсоров, чтобы снова открыть эти настройки.",
            )
            .size(11.0)
            .color(FAINT),
        );
    }

    /// Редактор общего действия (свайп / тап / нажатие метрики).
    fn tile_action_editor(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        action: &mut TileAction,
    ) -> bool {
        let before = action.clone();
        let label = action.label();
        egui::ComboBox::from_id_salt(id.with("tile-action"))
            .selected_text(label)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(action, TileAction::NoAction, "Не назначено");
                ui.selectable_value(action, TileAction::PrevPage, "Предыдущая страница");
                ui.selectable_value(action, TileAction::NextPage, "Следующая страница");
                ui.separator();
                for media in MediaAction::ALL {
                    ui.selectable_value(action, TileAction::Media(media), media.label());
                }
                ui.separator();
                ui.selectable_value(
                    action,
                    TileAction::Hotkey {
                        key: "SPACE".into(),
                        ctrl: false,
                        alt: false,
                        shift: false,
                        win: false,
                    },
                    "Горячая клавиша",
                );
                ui.selectable_value(
                    action,
                    TileAction::Launch {
                        path: "taskmgr".into(),
                    },
                    "Диспетчер задач",
                );
                ui.selectable_value(
                    action,
                    TileAction::Launch {
                        path: String::new(),
                    },
                    "Открыть URL / программу",
                );
            });

        match action {
            TileAction::Hotkey {
                key,
                ctrl,
                alt,
                shift,
                win,
            } => {
                ui.add_space(6.0);
                let mut next_key = key.clone();
                egui::ComboBox::from_id_salt(id.with("tile-hotkey-key"))
                    .selected_text(&next_key)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for choice in hotkey_choices() {
                            ui.selectable_value(&mut next_key, choice.to_string(), choice);
                        }
                    });
                *key = next_key;
                ui.horizontal(|ui| {
                    ui.checkbox(ctrl, "Ctrl");
                    ui.checkbox(alt, "Alt");
                    ui.checkbox(shift, "Shift");
                    ui.checkbox(win, "Win");
                });
            }
            TileAction::Launch { path } => {
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(path)
                        .desired_width(f32::INFINITY)
                        .hint_text("https://…  или  C:\\Path\\app.exe  или  taskmgr"),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("taskmgr").clicked() {
                        *path = "taskmgr".into();
                    }
                    if ui.small_button("https://").clicked() && !path.starts_with("http") {
                        *path = "https://".into();
                    }
                });
            }
            _ => {}
        }

        *action != before
    }

    fn encoder_inspector(&mut self, ui: &mut egui::Ui, index: u8) {
        let Some(cfg) = self.page().and_then(|p| p.encoders.iter().find(|e| e.index == index)).cloned() else {
            ui.label("Настройки крутилки недоступны."); return;
        };
        setting_title(ui, "Привязка к сенсору", "Выберите сенсор под которым будет отображаться эта крутилка. Выберите «Без привязки», чтобы использовать её отдельно.");
        let mut sensor = cfg.sensor_index;
        egui::ComboBox::from_id_salt(("encoder-sensor", index))
            .selected_text(sensor.map(|s| format!("Сенсор {s}")).unwrap_or_else(|| "Без привязки".into()))
            .width(ui.available_width()).show_ui(ui, |ui| {
                ui.selectable_value(&mut sensor, None, "Без привязки");
                for slot in 1..=4 { ui.selectable_value(&mut sensor, Some(slot), format!("Сенсор {slot}")); }
            });
        if sensor != cfg.sensor_index {
            if let Some(page) = self.page_mut() { page.bind_encoder_sensor(index, sensor); }
            self.dirty = true;
        }
        setting_title(ui, "Шаг изменения громкости", "На сколько процентов меняется громкость при каждом шаге энкодера.");
        let mut step = cfg.step_percent;
        if slider_row_u8(ui, &mut step, 1..=10, "% за шаг") {
            if let Some(e) = self.page_mut().and_then(|p| p.encoders.iter_mut().find(|e| e.index == index)) { e.step_percent = step; }
            self.dirty = true;
        }
        for (label, clockwise) in [("Вращение вправо", true), ("Вращение влево", false)] {
            setting_title(ui, label, if clockwise { "Действие при вращении энкодера вправо." } else { "Действие при вращении энкодера влево." });
            let mut binding = if clockwise { cfg.clockwise } else { cfg.counter_clockwise };
            egui::ComboBox::from_id_salt(("encoder-binding", index, clockwise)).selected_text(match binding {
                EncoderBinding::Volume => "Громкость по направлению",
                EncoderBinding::VolumeUp => "Прибавить громкость",
                EncoderBinding::VolumeDown => "Убавить громкость",
                EncoderBinding::NoAction => "Не назначено",
                EncoderBinding::Media(action) => action.label(),
            }).width(ui.available_width()).show_ui(ui, |ui| {
                ui.selectable_value(&mut binding, EncoderBinding::VolumeUp, "Прибавить громкость");
                ui.selectable_value(&mut binding, EncoderBinding::VolumeDown, "Убавить громкость");
                ui.selectable_value(&mut binding, EncoderBinding::Volume, "Громкость по направлению");
                ui.selectable_value(&mut binding, EncoderBinding::NoAction, "Не назначено");
                for action in MediaAction::ALL { ui.selectable_value(&mut binding, EncoderBinding::Media(action), action.label()); }
            });
            let current = if clockwise { cfg.clockwise } else { cfg.counter_clockwise };
            if binding != current {
                if let Some(e) = self.page_mut().and_then(|p| p.encoders.iter_mut().find(|e| e.index == index)) {
                    if clockwise { e.clockwise = binding } else { e.counter_clockwise = binding }
                }
                self.dirty = true;
            }
        }
        setting_title(ui, "Нажатие крутилки", "Действие при нажатии на энкодер.");
        let mut press = cfg.press_action.clone();
        egui::ComboBox::from_id_salt(("encoder-press", index)).selected_text(match &press { EncoderPressAction::MuteApplication => "Mute / выключить звук".to_owned(), EncoderPressAction::NoAction => "Не назначено".to_owned(), EncoderPressAction::Media(action) => action.label().to_owned(), EncoderPressAction::Hotkey { .. } => "Горячая клавиша".to_owned() }).width(ui.available_width()).show_ui(ui, |ui| {
            ui.selectable_value(&mut press, EncoderPressAction::MuteApplication, "Mute / выключить звук");
            ui.selectable_value(&mut press, EncoderPressAction::NoAction, "Не назначено");
            for action in MediaAction::ALL { ui.selectable_value(&mut press, EncoderPressAction::Media(action), action.label()); }
            ui.selectable_value(&mut press, EncoderPressAction::Hotkey { key: String::new(), ctrl: false, alt: false, shift: false, win: false }, "Горячая клавиша");
        });
        if let EncoderPressAction::Hotkey { key, ctrl, alt, shift, win } = &mut press {
            setting_title(ui, "Клавиша", "Например, F13, SPACE или M. Модификаторы отмечаются ниже.");
            ui.add(egui::TextEdit::singleline(key).desired_width(ui.available_width()).hint_text("Например, F13"));
            ui.horizontal(|ui| {
                for (label, value) in [("Ctrl", ctrl), ("Alt", alt), ("Shift", shift), ("Win", win)] {
                    ui.checkbox(value, label);
                }
            });
        }
        if press != cfg.press_action { if let Some(e) = self.page_mut().and_then(|p| p.encoders.iter_mut().find(|e| e.index == index)) { e.press_action = press; } self.dirty = true; }
    }

    fn device_inspector(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (tab, label) in [(SettingsTab::General, "Общие"), (SettingsTab::Mixers, "Микшеры"), (SettingsTab::Log, "Журнал")] {
                let selected = self.settings_tab == tab;
                let response = ui.selectable_label(selected, label);
                if response.clicked() { self.settings_tab = tab; }
            }
        });
        ui.separator();
        ui.add_space(10.0);
        match self.settings_tab {
            SettingsTab::General => {
                let gap = ui.spacing().item_spacing.x;
                let col_width = (ui.available_width() - gap * 2.0) / 3.0;
                ui.columns(3, |columns| {
                    for column in &mut columns[..] { column.set_width(col_width); }
                    section_label(&mut columns[0], "УСТРОЙСТВО");
                    setting_title(&mut columns[0], "Яркость экрана", "Уровень яркости дисплея устройства.");
                    if slider_row_u8(&mut columns[0], &mut self.draft.device.brightness, 0..=100, "%") { self.dirty = true; }
                    columns[0].add_space(14.0);
                    setting_title(&mut columns[0], "Цвет цифр", "Цвет значений на цифровых плитках.");
                    let digit = self.draft.device.digit_color;
                    let mut digit_picker = Color32::from_rgb(digit[0], digit[1], digit[2]);
                    if columns[0].color_edit_button_srgba(&mut digit_picker).changed() {
                        self.draft.device.digit_color = [digit_picker.r(), digit_picker.g(), digit_picker.b()];
                        self.dirty = true;
                        self.tex_dirty = true;
                    }
                    columns[0].add_space(10.0);
                    setting_title(&mut columns[0], "Цвет полосы", "Цвет полос и колец без градиента.");
                    let bar = self.draft.device.bar_color;
                    let mut bar_picker = Color32::from_rgb(bar[0], bar[1], bar[2]);
                    if columns[0].color_edit_button_srgba(&mut bar_picker).changed() {
                        self.draft.device.bar_color = [bar_picker.r(), bar_picker.g(), bar_picker.b()];
                        self.dirty = true;
                        self.tex_dirty = true;
                    }
                    section_label(&mut columns[1], "ПРОИЗВОДИТЕЛЬНОСТЬ");
                    setting_title(&mut columns[1], "Частота обновления экрана", "Чем больше герц, тем чаще обновляется картинка на устройстве при вращении крутилки и анимации.");
                    if slider_row_u32(&mut columns[1], &mut self.draft.device.refresh_hz, 1..=60, " Гц") {
                        self.dirty = true;
                    }
                    columns[1].add_space(14.0);
                    section_label(&mut columns[1], "СИСТЕМА");
                    setting_title(
                        &mut columns[1],
                        "Автозапуск",
                        "Запускать akp-deck при входе в Windows. Может появиться запрос UAC.",
                    );
                    let mut autostart = self.draft.app.autostart;
                    if columns[1]
                        .checkbox(&mut autostart, "Запускать вместе с Windows")
                        .changed()
                    {
                        self.draft.app.autostart = autostart;
                        if let Err(error) = crate::autostart::apply(autostart) {
                            self.save_error = Some(format!("Автозапуск: {error}"));
                        }
                        self.dirty = true;
                    }
                    section_label(&mut columns[2], "МОНИТОРИНГ");
                    setting_title(&mut columns[2], "Общий интервал мониторинга", "Как часто приложение считывает системные метрики.");
                    if slider_row_u64(&mut columns[2], &mut self.draft.monitor.interval_ms, 250..=10_000, " мс", true) { self.dirty = true; }
                    columns[2].add_space(14.0);
                    setting_title(&mut columns[2], "Градиенты метрик", "Только для мониторинга: зелёный / оранжевый / красный пороги.");
                    egui::ScrollArea::vertical().id_salt("gradient_scroll").max_height(220.0).show(&mut columns[2], |ui| {
                        for metric in Metric::ALL.into_iter().filter(|metric| metric.supports_gradient()) {
                            let entry = self.draft.monitor.gradients.iter().find(|item| item.metric == metric).map(|item| item.gradient)
                                .unwrap_or_else(|| config::default_gradient(metric));
                            let mut green = entry.green;
                            let mut orange = entry.orange;
                            let mut red = entry.red;
                            ui.label(egui::RichText::new(metric.label()).size(11.0).color(TEXT));
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("G").size(10.0).color(GREEN));
                                let mut changed = ui.add(egui::DragValue::new(&mut green).speed(0.5).range(0.0..=200.0)).changed();
                                ui.label(egui::RichText::new("O").size(10.0).color(AMBER));
                                changed |= ui.add(egui::DragValue::new(&mut orange).speed(0.5).range(0.0..=200.0)).changed();
                                ui.label(egui::RichText::new("R").size(10.0).color(RED));
                                changed |= ui.add(egui::DragValue::new(&mut red).speed(0.5).range(0.0..=200.0)).changed();
                                if changed {
                                    if let Some(item) = self.draft.monitor.gradients.iter_mut().find(|item| item.metric == metric) {
                                        item.gradient = config::Gradient { green, orange, red };
                                    } else {
                                        self.draft.monitor.gradients.push(config::MetricGradient {
                                            metric,
                                            gradient: config::Gradient { green, orange, red },
                                        });
                                    }
                                    self.dirty = true;
                                    self.tex_dirty = true;
                                }
                            });
                            ui.add_space(4.0);
                        }
                    });
                    columns[2].add_space(14.0);
                    if columns[2].add_sized([col_width, 34.0], egui::Button::new(egui::RichText::new("Вернуть значения по умолчанию").size(11.5).color(MUTED))).clicked() {
                        self.draft = Config::default(); self.selected_scene = 0; self.selected_page = 0; self.dirty = true; self.tex_dirty = true; self.style_key = None;
                    }
                });
            }
            SettingsTab::Mixers => self.mixers_tab(ui),
            SettingsTab::Log => {
                let log = self.shared.lock().map(|s| s.log.iter().rev().take(120).cloned().collect::<Vec<_>>()).unwrap_or_default();
                egui::Frame::new().fill(Color32::from_rgb(10, 12, 17)).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(12)).show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt("log_scroll").max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
                        if log.is_empty() { ui.label(egui::RichText::new("Журнал пока пуст").size(11.0).color(FAINT)); }
                        for line in &log { ui.label(egui::RichText::new(line).size(10.5).monospace().color(MUTED)); }
                    });
                });
            }
        }
    }

    fn mixers_tab(&mut self, ui: &mut egui::Ui) {
        setting_title(ui, "Все микшеры", "Список всех когда-либо увиденных аудиосессий. Скрытые не доступны в выпадающем списке и игнорируются динамическим захватом.");
        ui.add_space(8.0);
        let live: HashSet<String> = {
            let audio = self.shared.lock().ok().map(|s| Arc::clone(&s.audio));
            audio.and_then(|audio| audio.lock().ok().map(|state| state.sessions.iter().map(|s| s.app.clone()).collect())).unwrap_or_default()
        };
        if self.draft.audio.mixers.is_empty() {
            ui.label(egui::RichText::new("Пока нет известных микшеров. Они появятся, когда Windows отдаст аудиосессии.").size(11.5).color(MUTED));
            return;
        }
        egui::ScrollArea::vertical().id_salt("mixers_scroll").max_height(420.0).show(ui, |ui| {
            let mixers = self.draft.audio.mixers.clone();
            for entry in mixers {
                egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(10)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if let Some(icon) = self.audio_icon_textures.get(&entry.app) {
                            ui.add(egui::Image::new((icon.id(), Vec2::splat(22.0))));
                        }
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(if entry.name.is_empty() { entry.app.clone() } else { entry.name.clone() }).size(12.5).color(TEXT));
                            ui.label(egui::RichText::new(format!("{}{}", entry.app, if live.contains(&entry.app) { " · активен" } else { " · нет сессии" })).size(10.5).color(FAINT));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let hidden = entry.hidden;
                            let label = if hidden { "Показать" } else { "Скрыть" };
                            let tip = if hidden {
                                "Микшер скрыт: не в списке и не для динамического захвата"
                            } else {
                                "Скрыть из списка и динамического захвата"
                            };
                            if ui.add(egui::Button::new(egui::RichText::new(label).size(11.0).color(if hidden { AMBER } else { MUTED })).fill(if hidden { CARD_3 } else { CARD_2 })).on_hover_text(tip).clicked() {
                                if self.draft.audio.set_hidden(&entry.app, !hidden) {
                                    self.dirty = true;
                                    self.tex_dirty = true;
                                    self.last_dynamic_at = Instant::now() - Duration::from_secs(5);
                                }
                            }
                        });
                    });
                });
                ui.add_space(6.0);
            }
        });
    }

    /// Свои цвета плитки: галочка включает цвет цифр и полосы вместо общих.
    fn tile_colors_editor(&mut self, ui: &mut egui::Ui, for_metric: bool, metric: Metric) {
        ui.add_space(14.0);
        section_label(ui, "ЦВЕТА");
        let colors = match self.selected {
            Some(Selection::Key(index)) => self.key_cfg(index).map(|key| *key.colors()),
            Some(Selection::Zone(index)) => self.page().and_then(|p| p.zones.iter().find(|z| z.index == index)).map(|z| z.colors),
            _ => None,
        };
        let Some(colors) = colors else { return };
        let mut custom = colors.digit.is_some() || colors.bar.is_some();
        if ui.checkbox(&mut custom, "Свои цвета для этой плитки").changed() {
            let next = if custom {
                TileColors {
                    digit: Some(self.draft.device.digit_color),
                    bar: Some(self.draft.device.bar_color),
                    gradient: colors.gradient,
                }
            } else {
                TileColors { digit: None, bar: None, gradient: colors.gradient }
            };
            self.write_tile_colors(next);
        }
        if custom {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Цифры");
                let mut digit = Color32::from_rgb(colors.digit.unwrap_or(self.draft.device.digit_color)[0], colors.digit.unwrap_or(self.draft.device.digit_color)[1], colors.digit.unwrap_or(self.draft.device.digit_color)[2]);
                if ui.color_edit_button_srgba(&mut digit).changed() {
                    let mut next = colors;
                    next.digit = Some([digit.r(), digit.g(), digit.b()]);
                    self.write_tile_colors(next);
                }
                ui.add_space(12.0);
                ui.label("Полоса");
                let mut bar = Color32::from_rgb(colors.bar.unwrap_or(self.draft.device.bar_color)[0], colors.bar.unwrap_or(self.draft.device.bar_color)[1], colors.bar.unwrap_or(self.draft.device.bar_color)[2]);
                if ui.color_edit_button_srgba(&mut bar).changed() {
                    let mut next = colors;
                    next.bar = Some([bar.r(), bar.g(), bar.b()]);
                    self.write_tile_colors(next);
                }
            });
        }
        if for_metric && metric.supports_gradient() {
            ui.add_space(8.0);
            let mut use_grad = colors.gradient.is_some();
            if ui.checkbox(&mut use_grad, "Свой градиент метрики").changed() {
                let mut next = colors;
                next.gradient = if use_grad {
                    Some(self.draft.monitor.gradient(metric).unwrap_or_else(|| config::default_gradient(metric)))
                } else {
                    None
                };
                self.write_tile_colors(next);
            }
            if let Some(mut gradient) = colors.gradient {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("G").color(GREEN));
                    let mut changed = ui.add(egui::DragValue::new(&mut gradient.green).speed(0.5).range(0.0..=200.0)).changed();
                    ui.label(egui::RichText::new("O").color(AMBER));
                    changed |= ui.add(egui::DragValue::new(&mut gradient.orange).speed(0.5).range(0.0..=200.0)).changed();
                    ui.label(egui::RichText::new("R").color(RED));
                    changed |= ui.add(egui::DragValue::new(&mut gradient.red).speed(0.5).range(0.0..=200.0)).changed();
                    if changed {
                        let mut next = colors;
                        next.gradient = Some(gradient);
                        self.write_tile_colors(next);
                    }
                });
            }
        }
    }

    fn write_tile_colors(&mut self, colors: TileColors) {
        match self.selected {
            Some(Selection::Key(index)) => {
                if let Some(key) = self.key_cfg_mut(index) {
                    *key.colors_mut() = colors;
                }
            }
            Some(Selection::Zone(index)) => {
                if let Some(zone) = self.page_mut().and_then(|p| p.zones.iter_mut().find(|z| z.index == index)) {
                    zone.colors = colors;
                }
            }
            _ => {}
        }
        self.dirty = true;
        self.tex_dirty = true;
    }
}

// ---------------------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------------------

impl eframe::App for ConfigApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.handle_tray(ui.ctx()) {
            return;
        }
        self.cell_drop_handled = false;
        let (snapshot_opt, volumes, muted, status, audio, active_page, config_error) = match self.shared.lock() {
            Ok(shared) => (
                shared.snapshot.clone(),
                shared.volumes,
                shared.muted,
                shared.status.clone(),
                Arc::clone(&shared.audio),
                shared.active_page,
                shared.config_error.clone(),
            ),
            Err(poisoned) => {
                let shared = poisoned.into_inner();
                (
                    shared.snapshot.clone(),
                    shared.volumes,
                    shared.muted,
                    shared.status.clone(),
                    Arc::clone(&shared.audio),
                    shared.active_page,
                    shared.config_error.clone(),
                )
            }
        };

        if !self.dirty && active_page < self.pages().len() && active_page != self.selected_page {
            self.selected_page = active_page;
            self.tex_dirty = true;
            self.style_key = None;
        }

        let audio_state = match audio.lock() {
            Ok(state) => state.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        self.refresh_audio_icons(ui.ctx(), &audio_state);

        // Подтягиваем живые сессии в список известных микшеров.
        let sessions: Vec<(&str, &str, &str)> = audio_state
            .sessions
            .iter()
            .map(|session| (session.app.as_str(), session.name.as_str(), session.exe_path.as_str()))
            .collect();
        let _ = self.draft.audio.merge_sessions(sessions.iter().copied());
        if !self.dirty {
            if let Ok(shared) = self.shared.lock() {
                // Скрытые флаги и динамические назначения с устройства.
                for entry in &shared.config.audio.mixers {
                    if let Some(local) = self.draft.audio.mixers.iter_mut().find(|item| item.app == entry.app) {
                        local.hidden = entry.hidden;
                    }
                }
                self.draft.scenes = shared.config.scenes.clone();
                self.saved_config.audio.mixers = self.draft.audio.mixers.clone();
                self.saved_config.scenes = self.draft.scenes.clone();
            }
        }
        if self.last_dynamic_at.elapsed() >= Duration::from_secs(5) {
            self.last_dynamic_at = Instant::now();
            if self.draft.resolve_dynamic_apps(sessions.iter().copied()) {
                self.tex_dirty = true;
                if !self.dirty {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.config.scenes = self.draft.scenes.clone();
                        shared.config.audio.mixers = self.draft.audio.mixers.clone();
                    }
                    self.saved_config.scenes = self.draft.scenes.clone();
                }
            }
        }

        let snapshot = snapshot_opt.clone().unwrap_or_default();
        self.refresh_tiles(ui.ctx(), &snapshot, volumes, muted, &audio_state);

        // Горячие клавиши.
        let (escape, apply_hotkey) = ui.ctx().input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.modifiers.command && i.key_pressed(egui::Key::S),
            )
        });
        if escape && self.global_settings {
            self.global_settings = false;
        } else if escape && self.inspector_open {
            self.inspector_open = false;
        }
        if apply_hotkey && self.dirty {
            self.apply();
        }

        let undo_hotkey = !ui.ctx().egui_wants_keyboard_input() && ui.ctx().input(|i| i.modifiers.command && i.key_pressed(egui::Key::Z));
        if undo_hotkey { self.undo(); }

        egui::Panel::left("scene_sidebar").resizable(false).exact_size(190.0).frame(
            egui::Frame::new().fill(HEADER).inner_margin(Margin::symmetric(12, 12))
        ).show(ui, |ui| {
            ui.horizontal(|ui| {
                logo(ui);
                if icon_button(ui, IconKind::Settings).on_hover_text("Настройки").clicked() { self.global_settings = !self.global_settings; }
                if icon_button(ui, IconKind::Undo).on_hover_text("Отменить последнее изменение").clicked() { self.undo(); }
            });
            ui.separator();
            for idx in 0..self.draft.scenes.len() {
                let name = self.draft.scenes[idx].name.clone();
                let (response, drag_response) = ui.horizontal(|ui| {
                    scene_glyph(ui);
                    let response = ui.selectable_label(idx == self.selected_scene, &name);
                    let drag = ui.dnd_drag_source(ui.scope_id().with(("scene-handle", idx)), SceneDrag(idx), |ui| {
                        icon_button(ui, IconKind::Grip).on_hover_cursor(CursorIcon::Grab).on_hover_text("Перетащить сцену");
                    });
                    (response, drag.response)
                }).inner;
                if response.clicked() {
                    self.selected_scene = idx; self.draft.active_scene = idx; self.selected_page = 0;
                    self.global_settings = false; self.dirty = true; self.tex_dirty = true; self.selected = Some(Selection::Library);
                }
                if egui::DragAndDrop::payload::<SceneDrag>(ui.ctx()).is_some_and(|payload| payload.0 != idx) {
                    ui.painter().rect_stroke(response.rect, CornerRadius::same(8), Stroke::new(1.5, ACCENT), StrokeKind::Inside);
                }
                response.context_menu(|ui| {
                    ui.spacing_mut().button_padding.x = 4.0;
                    if ui.button("Переименовать").clicked() { self.renaming_scene = Some(idx); ui.close(); }
                    if self.draft.scenes.len() > 1 && ui.button("Удалить сцену").clicked() { self.confirm_delete = Some(DeleteTarget::Scene(idx)); ui.close(); }
                });
                if let Some(payload) = response.dnd_release_payload::<SceneDrag>().or_else(|| drag_response.dnd_release_payload::<SceneDrag>()) {
                    let from = payload.0;
                    if from < self.draft.scenes.len() && from != idx {
                        let scene = self.draft.scenes.remove(from); self.draft.scenes.insert(idx, scene);
                        self.selected_scene = if self.selected_scene == from { idx } else if from < self.selected_scene && idx >= self.selected_scene { self.selected_scene - 1 } else if from > self.selected_scene && idx <= self.selected_scene { self.selected_scene + 1 } else { self.selected_scene };
                        self.draft.active_scene = self.selected_scene; self.dirty = true;
                    }
                }
                if self.renaming_scene == Some(idx) {
                    let mut value = self.draft.scenes[idx].name.clone();
                    let response = ui.add(egui::TextEdit::singleline(&mut value).desired_width(ui.available_width()));
                    if response.changed() { self.draft.scenes[idx].name = value; self.dirty = true; }
                    if response.lost_focus() { self.renaming_scene = None; }
                }
            }
            if plus_icon_button(ui).on_hover_text("Новая сцена").clicked() {
                let idx = self.draft.scenes.len();
                self.draft.scenes.push(crate::config::Scene { name: format!("Сцена {}", idx + 1), pages: vec![Page::default()] });
                self.selected_scene = idx; self.draft.active_scene = idx; self.selected_page = 0; self.dirty = true; self.tex_dirty = true;
            }
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.separator();
                egui::ComboBox::from_id_salt("device-selector").selected_text("AKP05E").width(ui.available_width()).show_ui(ui, |ui| { let _ = ui.selectable_label(true, "AKP05E"); });
                ui.label(egui::RichText::new("УСТРОЙСТВО").size(9.0).strong().color(FAINT));
            });
            if let Some(error) = &self.save_error { ui.add_space(8.0); ui.label(egui::RichText::new(error).size(10.0).color(RED)); }
            if let Some(error) = &config_error { ui.add_space(8.0); ui.label(egui::RichText::new(error).size(10.0).color(RED)); }
        });

        let mut open = true;
        let _shown = egui::Panel::right("inspector")
            .resizable(false)
            .exact_size(404.0)
            .show_separator_line(true)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(18, 14)),
            )
            .show_collapsible(ui, &mut open, |ui| {
                self.inspector(ui, &snapshot, volumes, muted, &audio_state, &status);
            });
        self.inspector_open = true;

        self.central(ui, snapshot_opt.as_ref(), volumes, muted);
        if self.global_settings {
            let modal = egui::Modal::new(ui.make_persistent_id("global-settings-modal"))
                .frame(egui::Frame::new().fill(PANEL).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(16)).inner_margin(Margin::same(20)))
                .show(ui.ctx(), |ui| {
                    let viewport = ui.ctx().content_rect().size();
                    let modal_width = (viewport.x * 0.75 - 40.0).max(420.0);
                    let modal_height = (viewport.y * 0.75 - 40.0).max(320.0);
                    ui.set_width(modal_width);
                    ui.set_min_size(Vec2::new(modal_width, modal_height));
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Настройки").size(18.0).strong().color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if close_icon_button(ui).on_hover_text("Закрыть").clicked() { self.global_settings = false; }
                        });
                    });
                    egui::ScrollArea::vertical().max_height((modal_height - 76.0).max(180.0)).show(ui, |ui| {
                        ui.spacing_mut().slider_width = ui.available_width().min(760.0).max(100.0);
                        self.device_inspector(ui);
                    });
                });
            if modal.should_close() { self.global_settings = false; }
        }
        if let Some(target) = self.confirm_delete {
            let confirm = egui::Modal::new(ui.make_persistent_id("confirm-delete-modal"))
                .frame(egui::Frame::new().fill(PANEL).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(18)))
                .show(ui.ctx(), |ui| {
                ui.set_width(360.0);
                ui.label(egui::RichText::new("Подтверждение удаления").size(16.0).strong());
                ui.add_space(6.0);
                ui.label("Удалить назначение? Это изменение сразу сохранится.");
                ui.horizontal(|ui| {
                    if ui.button(egui::RichText::new("Удалить").color(Color32::WHITE)).clicked() {
                        match target {
                            DeleteTarget::Key(index) => { if let Some(page) = self.page_mut() { page.keys.retain(|key| key.index() != index); } self.library_tab = LibraryTab::Button; self.selected = Some(Selection::Library); },
                            DeleteTarget::Zone(index) => { if let Some(page) = self.page_mut() { page.zones.retain(|zone| zone.index != index); for encoder in &mut page.encoders { if encoder.sensor_index == Some(index) { encoder.sensor_index = None; } } } self.library_tab = LibraryTab::Sensor; self.selected = Some(Selection::Library); },
                            DeleteTarget::Page(index) => { let mut new_len = None; if let Some(pages) = self.pages_mut() { if index > 0 && index < pages.len() { pages.remove(index); new_len = Some(pages.len()); } } if let Some(len) = new_len { self.selected_page = self.selected_page.min(len.saturating_sub(1)); } },
                            DeleteTarget::Scene(index) => if self.draft.scenes.len() > 1 && index < self.draft.scenes.len() { self.draft.scenes.remove(index); if self.selected_scene == index { self.selected_scene = self.selected_scene.min(self.draft.scenes.len() - 1); } else if self.selected_scene > index { self.selected_scene -= 1; } self.draft.active_scene = self.selected_scene; self.selected_page = 0; self.selected = Some(Selection::Library); },
                        }
                        self.confirm_delete = None; self.dirty = true; self.tex_dirty = true;
                    }
                    if ui.button("Отмена").clicked() { self.confirm_delete = None; }
                });
            });
            if confirm.should_close() { self.confirm_delete = None; }
        }
        let pointer_held = ui.ctx().input(|input| input.pointer.button_down(egui::PointerButton::Primary));
        let pointer_released = ui.ctx().input(|input| input.pointer.button_released(egui::PointerButton::Primary));
        if pointer_released && !self.cell_drop_handled {
            if let Some(payload) = egui::DragAndDrop::payload::<CellDrag>(ui.ctx()) {
                self.confirm_delete = Some(match *payload { CellDrag::Key(index) => DeleteTarget::Key(index), CellDrag::Zone(index) => DeleteTarget::Zone(index) });
            }
        }
        let typing = ui.ctx().input(|input| input.events.iter().any(|event| matches!(event, egui::Event::Text(_) | egui::Event::Key { .. })));
        if self.dirty && typing { self.apply_at = Some(Instant::now() + APPLY_DELAY); }
        if self.dirty && !pointer_held && self.apply_at.is_none_or(|at| Instant::now() >= at) { self.apply(); }

        let repaint = match self.apply_at {
            Some(at) if self.dirty => at.saturating_duration_since(Instant::now()).min(Duration::from_millis(900)),
            _ => Duration::from_millis(900),
        };
        ui.ctx().request_repaint_after(repaint);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.apply();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }
}

// ---------------------------------------------------------------------------
// Мелкие рисовалки
// ---------------------------------------------------------------------------

fn uv() -> Rect {
    Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0))
}

fn logo(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(10),
        ACCENT,
        Stroke::NONE,
        StrokeKind::Inside,
    );
    painter.rect(
        rect.shrink(1.0),
        CornerRadius::same(9),
        Color32::TRANSPARENT,
        Stroke::new(1.0, Color32::from_white_alpha(35)),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "K",
        FontId::proportional(19.0),
        Color32::WHITE,
    );
}

fn plus_icon_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    ui.painter().rect(rect, CornerRadius::same(9), if response.hovered() { CARD_3 } else { Color32::TRANSPARENT }, Stroke::new(1.0, if response.hovered() { BORDER } else { Color32::TRANSPARENT }), StrokeKind::Inside);
    let center = rect.center();
    let half = 6.0;
    ui.painter().line_segment([Pos2::new(center.x - half, center.y), Pos2::new(center.x + half, center.y)], Stroke::new(2.0, TEXT));
    ui.painter().line_segment([Pos2::new(center.x, center.y - half), Pos2::new(center.x, center.y + half)], Stroke::new(2.0, TEXT));
    response
}

fn grip_handle(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::drag());
    let color = if response.hovered() || response.dragged() { ACCENT_TEXT } else { MUTED };
    for row in -1..=1 { for col in -1..=1 { ui.painter().circle_filled(rect.center()+Vec2::new(col as f32*4.0,row as f32*4.0),1.1,color); } }
    response
}

fn scene_glyph(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 22.0), Sense::hover());
    let p = ui.painter();
    p.rect_stroke(Rect::from_min_size(rect.min + Vec2::new(3.0, 3.0), Vec2::new(12.0, 14.0)), CornerRadius::same(2), Stroke::new(1.2, ACCENT_TEXT), StrokeKind::Inside);
    p.rect_stroke(Rect::from_min_size(rect.min + Vec2::new(1.0, 5.0), Vec2::new(12.0, 14.0)), CornerRadius::same(2), Stroke::new(1.2, MUTED), StrokeKind::Inside);
}

#[derive(Clone, Copy)]
enum IconKind { Settings, Undo, Grip, Action, Cpu, Memory, Gpu, Network, Disk, System, Audio, Battery }

fn icon_button(ui: &mut egui::Ui, kind: IconKind) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::click());
    if response.hovered() { ui.painter().rect(rect, CornerRadius::same(9), CARD_3, Stroke::new(1.0, BORDER), StrokeKind::Inside); }
    let p = ui.painter();
    let c = rect.center();
    let color = if response.hovered() { TEXT } else { MUTED };
    match kind {
        IconKind::Settings => {
            p.circle_stroke(c, 6.0, Stroke::new(1.7, color)); p.circle_filled(c, 2.0, color);
            for i in 0..8 { let a = i as f32 * std::f32::consts::TAU / 8.0; let v = Vec2::new(a.cos(), a.sin()); p.line_segment([c + v * 7.0, c + v * 9.0], Stroke::new(2.0, color)); }
        }
        IconKind::Undo => {
            p.line_segment([c + Vec2::new(8.0, 0.0), c + Vec2::new(-7.0, 0.0)], Stroke::new(2.0, color));
            p.line_segment([c + Vec2::new(-7.0, 0.0), c + Vec2::new(-1.0, -6.0)], Stroke::new(2.0, color));
            p.line_segment([c + Vec2::new(-7.0, 0.0), c + Vec2::new(-1.0, 6.0)], Stroke::new(2.0, color));
            p.line_segment([c + Vec2::new(8.0, 0.0), c + Vec2::new(8.0, 6.0)], Stroke::new(1.5, color));
            p.line_segment([c + Vec2::new(8.0, 6.0), c + Vec2::new(3.0, 8.0)], Stroke::new(1.5, color));
        }
        IconKind::Grip => for row in -1..=1 { for col in -1..=1 { p.circle_filled(c + Vec2::new(col as f32 * 4.0, row as f32 * 4.0), 1.0, color); } },
        IconKind::Cpu => {
            p.rect_stroke(Rect::from_center_size(c, Vec2::splat(12.0)), CornerRadius::same(2), Stroke::new(1.5,color), StrokeKind::Inside);
            for i in -1..=1 { let d=i as f32*4.0; p.line_segment([c+Vec2::new(d,-9.0),c+Vec2::new(d,-6.0)],Stroke::new(1.3,color));p.line_segment([c+Vec2::new(d,6.0),c+Vec2::new(d,9.0)],Stroke::new(1.3,color)); }
            p.rect_filled(Rect::from_center_size(c,Vec2::splat(4.0)),CornerRadius::same(1),color);
        }
        IconKind::Memory => { p.rect_stroke(Rect::from_center_size(c,Vec2::new(17.0,9.0)),CornerRadius::same(2),Stroke::new(1.5,color),StrokeKind::Inside);for i in -2..=2 {p.line_segment([c+Vec2::new(i as f32*3.0,-3.0),c+Vec2::new(i as f32*3.0,3.0)],Stroke::new(1.0,color));} }
        IconKind::Gpu => { p.rect_stroke(Rect::from_center_size(c,Vec2::new(18.0,12.0)),CornerRadius::same(2),Stroke::new(1.5,color),StrokeKind::Inside);p.circle_stroke(c+Vec2::new(-3.0,0.0),3.2,Stroke::new(1.2,color));p.line_segment([c+Vec2::new(6.0,-3.0),c+Vec2::new(6.0,3.0)],Stroke::new(1.2,color)); }
        IconKind::Network => { p.circle_stroke(c,8.0,Stroke::new(1.4,color));p.line_segment([c+Vec2::new(-7.0,0.0),c+Vec2::new(7.0,0.0)],Stroke::new(1.1,color));p.circle_stroke(c,4.0,Stroke::new(1.0,color));p.line_segment([c+Vec2::new(0.0,-7.0),c+Vec2::new(0.0,7.0)],Stroke::new(1.0,color)); }
        IconKind::Disk => { p.rect_stroke(Rect::from_center_size(c,Vec2::new(16.0,14.0)),CornerRadius::same(2),Stroke::new(1.5,color),StrokeKind::Inside);p.circle_filled(c+Vec2::new(0.0,3.0),2.0,color);p.line_segment([c+Vec2::new(-5.0,-3.0),c+Vec2::new(5.0,-3.0)],Stroke::new(1.2,color)); }
        IconKind::System => { p.circle_stroke(c,7.0,Stroke::new(1.4,color));p.line_segment([c,c+Vec2::new(0.0,-4.0)],Stroke::new(1.4,color));p.line_segment([c,c+Vec2::new(4.0,2.0)],Stroke::new(1.4,color)); }
        IconKind::Audio => { p.add(egui::Shape::convex_polygon(vec![c+Vec2::new(-8.0,-3.0),c+Vec2::new(-4.0,-3.0),c+Vec2::new(1.0,-7.0),c+Vec2::new(1.0,7.0),c+Vec2::new(-4.0,3.0),c+Vec2::new(-8.0,3.0)],color,Stroke::NONE));p.circle_stroke(c+Vec2::new(-1.0,0.0),6.0,Stroke::new(1.2,color)); }
        IconKind::Battery => {
            p.rect_stroke(Rect::from_center_size(c, Vec2::new(14.0, 9.0)), CornerRadius::same(2), Stroke::new(1.5, color), StrokeKind::Inside);
            p.rect_filled(Rect::from_center_size(c + Vec2::new(-1.0, 0.0), Vec2::new(8.0, 5.0)), CornerRadius::same(1), color);
            p.rect_filled(Rect::from_center_size(c + Vec2::new(8.0, 0.0), Vec2::new(2.0, 4.0)), CornerRadius::same(1), color);
        }
        IconKind::Action => { p.rect_stroke(Rect::from_center_size(c,Vec2::new(15.0,13.0)),CornerRadius::same(3),Stroke::new(1.4,color),StrokeKind::Inside);p.line_segment([c+Vec2::new(-4.0,-2.0),c+Vec2::new(0.0,2.0)],Stroke::new(1.5,color));p.line_segment([c+Vec2::new(0.0,2.0),c+Vec2::new(5.0,-4.0)],Stroke::new(1.5,color)); }
    }
    response
}

fn component_row(ui: &mut egui::Ui, label: &str, kind: IconKind) -> egui::Response {
    let width = ui.available_width().max(80.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::click_and_drag());
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(8), if response.hovered() { CARD_3 } else { CARD_2 }, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    let c = rect.left_center() + Vec2::new(22.0, 0.0);
    let col = if response.hovered() { TEXT } else { ACCENT_TEXT };
    match kind {
        IconKind::Cpu => { painter.rect_stroke(Rect::from_center_size(c,Vec2::splat(12.0)),CornerRadius::same(2),Stroke::new(1.5,col),StrokeKind::Inside);painter.rect_filled(Rect::from_center_size(c,Vec2::splat(4.0)),CornerRadius::same(1),col); }
        IconKind::Memory => { painter.rect_stroke(Rect::from_center_size(c,Vec2::new(16.0,9.0)),CornerRadius::same(2),Stroke::new(1.5,col),StrokeKind::Inside);for i in -2..=2 {painter.line_segment([c+Vec2::new(i as f32*3.0,-3.0),c+Vec2::new(i as f32*3.0,3.0)],Stroke::new(1.0,col));} }
        IconKind::Gpu | IconKind::Disk => { painter.rect_stroke(Rect::from_center_size(c,Vec2::new(17.0,13.0)),CornerRadius::same(2),Stroke::new(1.5,col),StrokeKind::Inside);painter.circle_stroke(c,3.0,Stroke::new(1.1,col)); }
        IconKind::Network => { painter.circle_stroke(c,7.0,Stroke::new(1.4,col));painter.line_segment([c+Vec2::new(-6.0,0.0),c+Vec2::new(6.0,0.0)],Stroke::new(1.0,col));painter.line_segment([c+Vec2::new(0.0,-6.0),c+Vec2::new(0.0,6.0)],Stroke::new(1.0,col)); }
        IconKind::Audio => { painter.rect_filled(Rect::from_center_size(c,Vec2::new(7.0,9.0)),CornerRadius::same(1),col);painter.circle_stroke(c+Vec2::new(5.0,0.0),5.0,Stroke::new(1.2,col)); }
        IconKind::Battery => {
            painter.rect_stroke(Rect::from_center_size(c, Vec2::new(12.0, 8.0)), CornerRadius::same(2), Stroke::new(1.4, col), StrokeKind::Inside);
            painter.rect_filled(Rect::from_center_size(c + Vec2::new(-1.0, 0.0), Vec2::new(6.0, 4.0)), CornerRadius::same(1), col);
        }
        _ => { painter.rect_stroke(Rect::from_center_size(c,Vec2::new(14.0,12.0)),CornerRadius::same(3),Stroke::new(1.4,col),StrokeKind::Inside);painter.circle_filled(c,2.0,col); }
    }
    painter.text(Pos2::new(rect.left()+42.0,rect.center().y),Align2::LEFT_CENTER,label,FontId::proportional(12.5),TEXT);
    response
}

fn dot(ui: &mut egui::Ui, color: Color32, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 2.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}

fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(CARD_2)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(10.5).color(color));
        });
}

fn status_chip(ui: &mut egui::Ui, status: &str) {
    let connected = status.starts_with("подключено");
    let color = if connected { GREEN } else { AMBER };
    let label = if connected {
        "подключено".to_string()
    } else if status.is_empty() {
        "запуск…".to_string()
    } else {
        let short: String = status.chars().take(28).collect();
        if status.chars().count() > 28 {
            format!("{short}…")
        } else {
            short
        }
    };

    egui::Frame::new()
        .fill(CARD_2)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, color, 4.5);
                ui.label(egui::RichText::new(label).size(10.5).color(MUTED));
            });
        })
        .response
        .on_hover_text(status);
}

fn live_badge(ui: &mut egui::Ui) {
    let t = ui.input(|i| i.time) as f32;
    let pulse = 0.55 + 0.45 * (t * 2.2).sin().abs();
    let color = Color32::from_rgba_unmultiplied(
        53,
        208,
        127,
        (pulse * 255.0).clamp(0.0, 255.0) as u8,
    );

    egui::Frame::new()
        .fill(Color32::from_rgb(17, 33, 26))
        .stroke(Stroke::new(1.0, Color32::from_rgb(31, 68, 51)))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, color, 4.0);
                ui.label(
                    egui::RichText::new("в реальном времени")
                        .size(10.5)
                        .color(GREEN),
                );
            });
        });
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(10.0)
            .strong()
            .color(FAINT),
    );
}

fn setting_title(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(11.5).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
            ui.painter().circle_stroke(rect.center(), 8.5, Stroke::new(1.0, FAINT));
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, "?", FontId::proportional(11.0), MUTED);
            response.on_hover_text(description);
        });
    });
}

fn slider_row_u32(
    ui: &mut egui::Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    suffix: &str,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = (ui.available_width() - 72.0).max(80.0);
        changed = ui.add(egui::Slider::new(value, range).show_value(false)).changed();
        ui.label(egui::RichText::new(format!("{value}{suffix}")).size(11.5).color(MUTED));
    });
    changed
}

fn slider_row_u64(
    ui: &mut egui::Ui,
    value: &mut u64,
    range: std::ops::RangeInclusive<u64>,
    suffix: &str,
    logarithmic: bool,
) -> bool {
    let gap = ui.spacing().item_spacing.x;
    let width = (ui.available_width() - gap).max(80.0);
    let input_width = (width * 0.2).max(90.0).min(width * 0.5);
    let slider_width = width - input_width;
    let mut changed = false;
    ui.horizontal(|ui| {
        // Слайдер берёт ширину из spacing, а не из add_sized.
        ui.spacing_mut().slider_width = slider_width;
        let mut slider = egui::Slider::new(value, range.clone()).show_value(false).trailing_fill(true);
        if logarithmic { slider = slider.logarithmic(true); }
        let slider_changed = ui.add_sized([slider_width, 30.0], slider).changed();
        let input_changed = ui.add_sized([input_width, 30.0], egui::DragValue::new(value).range(range).suffix(suffix)).changed();
        changed = slider_changed || input_changed;
    });
    changed
}

fn slider_row_u8(
    ui: &mut egui::Ui,
    value: &mut u8,
    range: std::ops::RangeInclusive<u8>,
    suffix: &str,
) -> bool {
    let gap = ui.spacing().item_spacing.x;
    let width = (ui.available_width() - gap).max(80.0);
    let input_width = (width * 0.2).max(90.0).min(width * 0.5);
    let slider_width = width - input_width;
    let mut changed = false;
    ui.horizontal(|ui| {
        // Слайдер берёт ширину из spacing, а не из add_sized.
        ui.spacing_mut().slider_width = slider_width;
        let slider_changed = ui.add_sized([slider_width, 30.0], egui::Slider::new(value, range.clone()).show_value(false).trailing_fill(true)).changed();
        let input_changed = ui.add_sized([input_width, 30.0], egui::DragValue::new(value).range(range).suffix(suffix)).changed();
        changed = slider_changed || input_changed;
    });
    changed
}

fn close_icon_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    if response.hovered() { ui.painter().circle_filled(rect.center(), 15.0, CARD_3); }
    let center = rect.center();
    let half = 4.0;
    ui.painter().line_segment([Pos2::new(center.x - half, center.y - half), Pos2::new(center.x + half, center.y + half)], Stroke::new(1.7, TEXT));
    ui.painter().line_segment([Pos2::new(center.x + half, center.y - half), Pos2::new(center.x - half, center.y + half)], Stroke::new(1.7, TEXT));
    response
}

fn paint_tile(painter: &egui::Painter, tex: Option<&CachedTex>, rect: Rect) {
    match tex {
        Some(t) => {
            painter.image(t.texture.id(), rect, uv(), Color32::WHITE);
        }
        None => {
            painter.rect(
                rect,
                CornerRadius::ZERO,
                SCREEN,
                Stroke::NONE,
                StrokeKind::Inside,
            );
        }
    }
}

/// Человеческое имя приложения из ключа: `chrome.exe` → `Chrome`.
fn hotkey_choices() -> Vec<String> {
    let mut keys = Vec::with_capacity(60);
    keys.extend((b'A'..=b'Z').map(|byte| (byte as char).to_string()));
    keys.extend((b'0'..=b'9').map(|byte| (byte as char).to_string()));
    keys.extend((1..=12).map(|number| format!("F{number}")));
    keys.extend(
        ["Space", "Enter", "Tab", "Esc", "Up", "Down", "Left", "Right"]
            .into_iter()
            .map(str::to_string),
    );
    keys
}

fn app_display(app: &str) -> String {
    let stem = app.strip_suffix(".exe").unwrap_or(app);
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => app.to_string(),
    }
}

fn style_card(
    ui: &mut egui::Ui,
    tex: Option<&CachedTex>,
    label: &str,
    selected: bool,
) -> bool {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, width + 22.0), Sense::click());

    ui.painter().rect(
        rect,
        CornerRadius::same(11),
        if selected { ACCENT_DARK } else { CARD_2 },
        Stroke::new(1.0, if selected { ACCENT } else { BORDER }),
        StrokeKind::Inside,
    );

    let image = Rect::from_min_size(rect.min, Vec2::splat(width)).shrink(5.0);
    paint_tile(ui.painter(), tex, image);

    ui.painter().text(
        Pos2::new(rect.center().x, rect.bottom() - 11.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.0),
        if selected { ACCENT_TEXT } else { MUTED },
    );

    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp.clicked()
}

fn meter(ui: &mut egui::Ui, fraction: f32, color: Color32, height: f32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let radius = CornerRadius::same((height * 0.5) as u8);
    ui.painter()
        .rect(rect, radius, CARD_3, Stroke::NONE, StrokeKind::Inside);
    let filled = (width * fraction.clamp(0.0, 1.0)).round();
    if filled >= 2.0 {
        ui.painter().rect(
            Rect::from_min_size(rect.min, Vec2::new(filled, height)),
            radius,
            color,
            Stroke::NONE,
            StrokeKind::Inside,
        );
    }
}

fn arc(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    start_deg: f32,
    end_deg: f32,
    width: f32,
    color: Color32,
) {
    if end_deg <= start_deg {
        return;
    }
    let steps = 28;
    let points: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let angle = (start_deg + (end_deg - start_deg) * i as f32 / steps as f32)
                .to_radians();
            center + Vec2::new(angle.cos(), angle.sin()) * radius
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(width, color)));
}

fn stats_row(ui: &mut egui::Ui, snapshot: Option<&Snapshot>) {
    let default = Snapshot::default();
    let s = snapshot.unwrap_or(&default);
    let live = snapshot.is_some();

    ui.columns(4, |cols| {
        let cpu_temp = s
            .cpu_temp
            .map(|t| format!("{t:.0} °C"))
            .unwrap_or_else(|| "— °C".to_string());
        let cpu_frac = live.then_some((s.cpu_load / 100.0).clamp(0.0, 1.0));
        stat_card(
            &mut cols[0],
            "ПРОЦЕССОР",
            &if live { format!("{:.0}%", s.cpu_load) } else { "—".into() },
            "",
            &format!("{cpu_temp} · {:.2} ГГц", s.cpu_freq_mhz as f64 / 1000.0),
            cpu_frac,
            if s.cpu_load >= 85.0 { RED } else { ACCENT },
        );

        if let Some(gpu) = &s.gpu {
            let temp = gpu
                .temp_c
                .map(|t| format!("{t:.0} °C"))
                .unwrap_or_else(|| "— °C".to_string());
            let load = gpu.load.unwrap_or(0.0);
            stat_card(
                &mut cols[1],
                "ВИДЕОКАРТА",
                &if live { format!("{load:.0}%") } else { "—".into() },
                "",
                &format!("{temp} · {:.1} ГБ", gib(gpu.mem_used)),
                live.then_some((load / 100.0).clamp(0.0, 1.0)),
                if load >= 85.0 { RED } else { GREEN },
            );
        } else {
            stat_card(
                &mut cols[1],
                "ВИДЕОКАРТА",
                "—",
                "",
                "нет данных GPU",
                None,
                FAINT,
            );
        }

        stat_card(
            &mut cols[2],
            "ПАМЯТЬ",
            &if live { format!("{:.0}%", s.ram_percent) } else { "—".into() },
            "",
            &format!("{:.1} ГБ из {:.0} ГБ", gib(s.ram_used), gib(s.ram_total)),
            live.then_some((s.ram_percent / 100.0).clamp(0.0, 1.0)),
            if s.ram_percent >= 85.0 { AMBER } else { ACCENT },
        );

        stat_card(
            &mut cols[3],
            "СЕТЬ",
            &if live {
                format!("↓ {:.1}", s.net_down_bps as f64 / 1_000_000.0)
            } else {
                "—".into()
            },
            "Мбит/с",
            &format!(
                "↑ {:.1} Мбит/с · диск {:.0}%",
                s.net_up_bps as f64 / 1_000_000.0,
                s.disk_percent.unwrap_or(0.0)
            ),
            None,
            BLUE,
        );
    });
}

// Нужен отдельный алиас, чтобы не тянуть цвет устройства в палитру окна.
const BLUE: Color32 = ACCENT;

fn stat_card(
    ui: &mut egui::Ui,
    title: &str,
    value: &str,
    unit: &str,
    sub: &str,
    fraction: Option<f32>,
    color: Color32,
) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(9.5)
                    .strong()
                    .color(FAINT),
            );
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(value)
                        .size(23.0)
                        .strong()
                        .color(color),
                );
                if !unit.is_empty() {
                    ui.label(egui::RichText::new(unit).size(10.5).color(MUTED));
                }
            });
            ui.label(egui::RichText::new(sub).size(10.5).color(MUTED));
            if let Some(f) = fraction {
                ui.add_space(4.0);
                meter(ui, f, color, 4.0);
            }
        });
}

// ---------------------------------------------------------------------------
// Текстуры и хеши
// ---------------------------------------------------------------------------

fn put_texture(
    ctx: &egui::Context,
    slot: &mut Option<CachedTex>,
    name: String,
    img: image::RgbImage,
) {
    let hash = image_hash(&img);
    if slot.as_ref().is_some_and(|cached| cached.hash == hash) {
        return;
    }
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgb(size, img.as_raw());

    match slot {
        Some(cached) => {
            cached.texture.set(color, TextureOptions::LINEAR);
            cached.hash = hash;
        }
        None => {
            let texture = ctx.load_texture(name, color, TextureOptions::LINEAR);
            *slot = Some(CachedTex { hash, texture });
        }
    }
}

fn image_hash(img: &image::RgbImage) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in img.as_raw() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn snapshot_hash(s: &Snapshot, volumes: [i32; 4], muted: [bool; 4], audio: &AudioState) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;

    fn mix(h: &mut u64, v: u64) {
        *h ^= v;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }

    mix(&mut h, s.cpu_load.to_bits() as u64);
    mix(&mut h, s.cpu_temp.map(|v| v.to_bits() as u64).unwrap_or(u64::MAX));
    mix(&mut h, s.cpu_freq_mhz);
    mix(&mut h, s.ram_percent.to_bits() as u64);
    mix(&mut h, s.ram_used);
    mix(&mut h, s.ram_total);
    mix(&mut h, s.disk_percent.map(|v| v.to_bits() as u64).unwrap_or(u64::MAX));
    mix(&mut h, s.disk_used);
    mix(&mut h, s.disk_total);
    mix(&mut h, s.net_down_bps);
    mix(&mut h, s.net_up_bps);
    mix(&mut h, s.uptime_secs);
    for b in s.clock.bytes() {
        mix(&mut h, b as u64);
    }
    mix(&mut h, s.batteries.len() as u64);
    for device in &s.batteries {
        for b in device.id.bytes() {
            mix(&mut h, b as u64);
        }
        mix(&mut h, device.percent.unwrap_or(255) as u64);
        mix(&mut h, u64::from(device.charging));
    }
    match &s.gpu {
        Some(gpu) => {
            mix(&mut h, 1);
            mix(&mut h, gpu.temp_c.map(|v| v.to_bits() as u64).unwrap_or(u64::MAX));
            mix(&mut h, gpu.load.map(|v| v.to_bits() as u64).unwrap_or(u64::MAX));
            mix(&mut h, gpu.mem_used);
            mix(&mut h, gpu.mem_total);
            mix(&mut h, gpu.fan_percent.unwrap_or(0) as u64);
            for b in gpu.name.bytes() {
                mix(&mut h, b as u64);
            }
        }
        None => mix(&mut h, 2),
    }
    for v in volumes {
        mix(&mut h, v as u64);
    }
    for m in muted {
        mix(&mut h, m as u64);
    }

    for session in &audio.sessions {
        for byte in session.app.bytes() {
            mix(&mut h, byte as u64);
        }
        mix(&mut h, session.volume as u64);
        mix(&mut h, session.muted as u64);
    }
    mix(&mut h, audio.icons.len() as u64);
    mix(&mut h, audio.ready as u64);

    h
}
