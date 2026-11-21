//! Конфигурация приложения: %APPDATA%\akp-deck\config.toml

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub device: DeviceCfg,
    #[serde(default)]
    pub monitor: MonitorCfg,
    #[serde(default)]
    pub audio: AudioCfg,
    #[serde(default)]
    pub app: AppCfg,
    #[serde(default)]
    pub scenes: Vec<Scene>,
    #[serde(default)]
    pub active_scene: usize,
    /// Поле старой схемы. При загрузке его страницы переносятся в первую сцену.
    #[serde(default, skip_serializing)]
    pub pages: Vec<Page>,
}

/// Настройки приложения (не устройства).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppCfg {
    /// Запускать akp-deck при входе в Windows (HKCU Run).
    #[serde(default)]
    pub autostart: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    pub pages: Vec<Page>,
}

impl Config {
    pub fn active_scene(&self) -> Option<&Scene> {
        self.scenes.get(self.active_scene)
    }

    pub fn active_scene_mut(&mut self) -> Option<&mut Scene> {
        self.scenes.get_mut(self.active_scene)
    }

    pub fn active_pages(&self) -> &[Page] {
        self.active_scene().map(|scene| scene.pages.as_slice()).unwrap_or(&[])
    }

    /// Переносит старую схему с единым списком `pages` и нормализует индексы.
    pub fn migrate(&mut self) {
        if self.scenes.is_empty() {
            let pages = if self.pages.is_empty() {
                Config::default().active_pages().to_vec()
            } else {
                std::mem::take(&mut self.pages)
            };
            self.scenes.push(Scene { name: "Основная".into(), pages });
        }
        if self.scenes.iter().any(|scene| scene.pages.is_empty()) {
            for scene in &mut self.scenes {
                if scene.pages.is_empty() {
                    scene.pages.push(Page::default());
                }
                scene.pages.truncate(8);
            }
        }
        for scene in &mut self.scenes {
            scene.pages.truncate(8);
            for page in &mut scene.pages {
                if page.encoders.is_empty() {
                    page.encoders = default_encoders();
                    for sensor in &page.zones {
                        if let Some(encoder) = page.encoders.iter_mut().find(|item| item.index == sensor.index) {
                            encoder.step_percent = sensor.step_percent.clamp(1, 10);
                            encoder.press_action = sensor.press_action.clone();
                        }
                    }
                    page.zones.retain(|sensor| {
                        sensor.has_content()
                            || sensor.app_path.is_some()
                            || sensor.title != format!("VOL {}", sensor.index)
                    });
                }
                page.normalize_encoder_sensor_links();
            }
        }
        self.active_scene = self.active_scene.min(self.scenes.len().saturating_sub(1));
    }

    /// Назначает свободные микшеры зонам с `dynamic`. Не трогает скрытые и уже
    /// занятые другими сенсорами приложения. Возвращает `true`, если что-то изменилось.
    pub fn resolve_dynamic_apps<'a>(
        &mut self,
        sessions: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) -> bool {
        let sessions: Vec<(&str, &str, &str)> = sessions.into_iter().collect();
        let hidden: std::collections::HashSet<&str> = self
            .audio
            .mixers
            .iter()
            .filter(|entry| entry.hidden)
            .map(|entry| entry.app.as_str())
            .collect();

        let mut changed = false;
        for scene in &mut self.scenes {
            for page in &mut scene.pages {
                let mut taken = std::collections::HashSet::new();
                for zone in &page.zones {
                    if zone.dynamic {
                        continue;
                    }
                    if let Some(app) = zone.app.as_deref() {
                        taken.insert(app.to_string());
                    }
                }

                let mut dynamic_zones: Vec<usize> = page
                    .zones
                    .iter()
                    .enumerate()
                    .filter(|(_, zone)| zone.dynamic && zone.metric.is_none())
                    .map(|(index, _)| index)
                    .collect();
                dynamic_zones.sort_by_key(|index| page.zones[*index].index);

                for zone_index in dynamic_zones {
                    let current = page.zones[zone_index].app.clone();
                    let keep = current
                        .as_deref()
                        .is_some_and(|app| {
                            !hidden.contains(app)
                                && !taken.contains(app)
                                && sessions.iter().any(|(session_app, _, _)| *session_app == app)
                        });
                    if keep {
                        if let Some(app) = current {
                            taken.insert(app);
                        }
                        continue;
                    }

                    let pick = sessions.iter().find(|(app, _, _)| {
                        !app.is_empty() && !hidden.contains(app) && !taken.contains(*app)
                    });
                    match pick {
                        Some((app, name, path)) => {
                            let zone = &mut page.zones[zone_index];
                            if zone.app.as_deref() != Some(*app) {
                                zone.app = Some((*app).to_string());
                                zone.app_path = if path.is_empty() { None } else { Some((*path).to_string()) };
                                if zone.title.starts_with("VOL ") || zone.title.is_empty() || zone.title == "Динамический" {
                                    zone.title = if name.is_empty() { (*app).to_string() } else { (*name).to_string() };
                                }
                                changed = true;
                            }
                            taken.insert((*app).to_string());
                        }
                        None => {
                            let zone = &mut page.zones[zone_index];
                            if zone.app.take().is_some() || zone.app_path.take().is_some() {
                                if zone.title != "Динамический" {
                                    zone.title = "Динамический".into();
                                }
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        changed
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceCfg {
    pub brightness: u8,
    /// USB VID панели (по умолчанию AKP05E).
    #[serde(default = "default_vid")]
    pub vid: u16,
    /// USB PID панели. Другие модели mirajazz — через этот параметр.
    #[serde(default = "default_pid")]
    pub pid: u16,
    #[serde(default = "default_digit_color")]
    pub digit_color: [u8; 3],
    /// Цвет полос и колец, если для плитки не действует градиент.
    #[serde(default = "default_digit_color")]
    pub bar_color: [u8; 3],
    /// Минимальный интервал между отправкой изображений устройству.
    /// Низкая граница ограничивает очередь, пока устройство обрабатывает кадр.
    #[serde(default = "default_upload_gap")]
    pub upload_gap_ms: u64,
    /// Частота обновления картинки одной плитки, Гц: не чаще этого
    /// перерисовываются метрики и зона при вращении крутилки.
    #[serde(default = "default_refresh_hz")]
    pub refresh_hz: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorCfg {
    pub interval_ms: u64,
    /// Индекс GPU для NVML / LibreHardwareMonitor (0 — первая карта).
    #[serde(default)]
    pub gpu_index: u32,
    /// Общие границы градиента по метрикам. Метрики без записи окрашиваются
    /// цветами из `[device]`.
    #[serde(default = "default_gradients")]
    pub gradients: Vec<MetricGradient>,
}

impl MonitorCfg {
    pub fn gradient(&self, metric: Metric) -> Option<Gradient> {
        self.gradients.iter().find(|item| item.metric == metric).map(|item| item.gradient)
    }
}

/// Границы градиента: до `green` зелёный, к `orange` оранжевый, от `red` красный.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Gradient {
    pub green: f32,
    pub orange: f32,
    pub red: f32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct MetricGradient {
    pub metric: Metric,
    #[serde(flatten)]
    pub gradient: Gradient,
}

/// Свои цвета плитки. Свой градиент важнее своих цветов, свои цвета важнее
/// общего градиента метрики.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TileColors {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digit: Option<[u8; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bar: Option<[u8; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gradient: Option<Gradient>,
}

impl TileColors {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn default_upload_gap() -> u64 {
    120
}

fn default_refresh_hz() -> u32 {
    30
}

fn default_digit_color() -> [u8; 3] { [74, 158, 255] }

fn default_vid() -> u16 {
    0x0300
}

fn default_pid() -> u16 {
    0x3004
}

/// Границы по умолчанию для метрик, которые поддерживают градиент.
pub fn default_gradient(metric: Metric) -> Gradient {
    match metric {
        Metric::CpuTemp | Metric::GpuTemp => Gradient { green: 30.0, orange: 60.0, red: 80.0 },
        Metric::DiskUsed => Gradient { green: 70.0, orange: 85.0, red: 95.0 },
        // Для батареи в render передаётся (100 - %), поэтому пороги «как для температуры».
        Metric::DeviceBattery => Gradient { green: 50.0, orange: 75.0, red: 90.0 },
        _ => Gradient { green: 50.0, orange: 80.0, red: 95.0 },
    }
}

fn default_gradients() -> Vec<MetricGradient> {
    Metric::ALL
        .into_iter()
        .filter(|metric| metric.supports_gradient())
        .map(|metric| MetricGradient { metric, gradient: default_gradient(metric) })
        .collect()
}

impl Default for DeviceCfg {
    fn default() -> Self {
        Self {
            brightness: 80,
            vid: default_vid(),
            pid: default_pid(),
            digit_color: default_digit_color(),
            bar_color: default_digit_color(),
            upload_gap_ms: default_upload_gap(),
            refresh_hz: default_refresh_hz(),
        }
    }
}

impl Default for MonitorCfg {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            gpu_index: 0,
            gradients: default_gradients(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioCfg {
    /// Изменение громкости в процентах за один щелчок энкодера.
    #[serde(default = "default_volume_step")]
    pub step_percent: u8,
    /// Все когда-либо увиденные микшеры. Скрытые не показываются в списке
    /// приложений и игнорируются динамическим захватом.
    #[serde(default)]
    pub mixers: Vec<MixerEntry>,
}

/// Запись микшера Windows, сохранённая между запусками.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MixerEntry {
    pub app: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub exe_path: String,
    #[serde(default)]
    pub hidden: bool,
}

fn default_volume_step() -> u8 {
    5
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self {
            step_percent: default_volume_step(),
            mixers: Vec::new(),
        }
    }
}

impl AudioCfg {
    pub fn is_hidden(&self, app: &str) -> bool {
        self.mixers.iter().any(|entry| entry.app == app && entry.hidden)
    }

    /// Добавляет или обновляет живые сессии в списке известных микшеров.
    /// Возвращает `true`, если список изменился.
    pub fn merge_sessions<'a>(
        &mut self,
        sessions: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) -> bool {
        let mut changed = false;
        for (app, name, exe_path) in sessions {
            if app.is_empty() {
                continue;
            }
            if let Some(entry) = self.mixers.iter_mut().find(|entry| entry.app == app) {
                if entry.name != name && !name.is_empty() {
                    entry.name = name.to_string();
                    changed = true;
                }
                if entry.exe_path != exe_path && !exe_path.is_empty() {
                    entry.exe_path = exe_path.to_string();
                    changed = true;
                }
            } else {
                self.mixers.push(MixerEntry {
                    app: app.to_string(),
                    name: if name.is_empty() { app.to_string() } else { name.to_string() },
                    exe_path: exe_path.to_string(),
                    hidden: false,
                });
                changed = true;
            }
        }
        if changed {
            self.mixers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.app.cmp(&b.app)));
        }
        changed
    }

    pub fn set_hidden(&mut self, app: &str, hidden: bool) -> bool {
        if let Some(entry) = self.mixers.iter_mut().find(|entry| entry.app == app) {
            if entry.hidden != hidden {
                entry.hidden = hidden;
                return true;
            }
            return false;
        }
        false
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    pub name: String,
    pub keys: Vec<KeyCfg>,
    #[serde(default)]
    pub zones: Vec<ZoneCfg>,
    #[serde(default)]
    pub encoders: Vec<EncoderCfg>,
    /// Свайп влево по тач-стрипу.
    #[serde(default = "default_swipe_left")]
    pub swipe_left: TileAction,
    /// Свайп вправо по тач-стрипу.
    #[serde(default = "default_swipe_right")]
    pub swipe_right: TileAction,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            name: "Страница 1".into(),
            keys: Vec::new(),
            zones: Vec::new(),
            encoders: default_encoders(),
            swipe_left: default_swipe_left(),
            swipe_right: default_swipe_right(),
        }
    }
}

fn default_swipe_left() -> TileAction {
    TileAction::PrevPage
}

fn default_swipe_right() -> TileAction {
    TileAction::NextPage
}

impl Page {
    /// Keeps each sensor assigned to at most one encoder. Earlier links win
    /// when loading a config that already contains duplicates.
    pub fn normalize_encoder_sensor_links(&mut self) {
        for current in 0..self.encoders.len() {
            let Some(sensor) = self.encoders[current].sensor_index else { continue };
            if self.encoders[..current].iter().any(|encoder| encoder.sensor_index == Some(sensor)) {
                self.encoders[current].sensor_index = None;
            }
        }
    }

    pub fn bind_encoder_sensor(&mut self, encoder_index: u8, sensor_index: Option<u8>) {
        if let Some(sensor) = sensor_index {
            for encoder in &mut self.encoders {
                if encoder.index != encoder_index && encoder.sensor_index == Some(sensor) {
                    encoder.sensor_index = None;
                }
            }
        }
        if let Some(encoder) = self.encoders.iter_mut().find(|encoder| encoder.index == encoder_index) {
            encoder.sensor_index = sensor_index;
        }
    }

    /// Переносит привязки крутилок вместе с сенсором при перемещении ячейки.
    /// При обмене двух занятых слотов привязки тоже меняются местами.
    pub fn move_encoder_links(&mut self, from: u8, to: u8, destination_occupied: bool) {
        if from == to {
            return;
        }
        if destination_occupied {
            for encoder in &mut self.encoders {
                match encoder.sensor_index {
                    Some(link) if link == from => encoder.sensor_index = Some(to),
                    Some(link) if link == to => encoder.sensor_index = Some(from),
                    _ => {}
                }
            }
            return;
        }
        // Перенос в пустой слот: следуем за сенсором; старый целевой линк сбрасываем,
        // если кто-то «поехал» с `from` (не оставляем двух крутилок на одном слоте).
        let followed = self.encoders.iter().any(|encoder| encoder.sensor_index == Some(from));
        for encoder in &mut self.encoders {
            match encoder.sensor_index {
                Some(link) if link == from => encoder.sensor_index = Some(to),
                Some(link) if link == to && followed => encoder.sensor_index = None,
                _ => {}
            }
        }
    }

    /// Если сенсор ещё без крутилки — привязывает первую свободную
    /// (с тем же номером, иначе любую без привязки).
    pub fn auto_bind_free_encoder(&mut self, sensor: u8) -> bool {
        if self.encoders.iter().any(|encoder| encoder.sensor_index == Some(sensor)) {
            return false;
        }
        let preferred = self
            .encoders
            .iter()
            .find(|encoder| encoder.index == sensor && encoder.sensor_index.is_none())
            .map(|encoder| encoder.index)
            .or_else(|| {
                self.encoders
                    .iter()
                    .find(|encoder| encoder.sensor_index.is_none())
                    .map(|encoder| encoder.index)
            });
        let Some(encoder_index) = preferred else {
            return false;
        };
        self.bind_encoder_sensor(encoder_index, Some(sensor));
        true
    }

    /// Приложения, уже занятые другими сенсорами этой страницы.
    pub fn used_apps(&self, except_zone: Option<u8>) -> std::collections::HashSet<String> {
        self.zones
            .iter()
            .filter(|zone| except_zone != Some(zone.index))
            .filter_map(|zone| zone.app.clone())
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncoderCfg {
    pub index: u8,
    /// Sensor slot controlled by this encoder; None means standalone.
    #[serde(default)]
    pub sensor_index: Option<u8>,
    #[serde(default = "default_encoder_step")]
    pub step_percent: u8,
    #[serde(default)]
    pub clockwise: EncoderBinding,
    #[serde(default)]
    pub counter_clockwise: EncoderBinding,
    #[serde(default)]
    pub press_action: EncoderPressAction,
}

fn default_encoders() -> Vec<EncoderCfg> {
    (1..=4)
        .map(|index| EncoderCfg {
            index,
            // На новой странице крутилки свободны — привязка появляется при
            // перетаскивании «Громкость приложения» на сенсор.
            sensor_index: None,
            step_percent: 5,
            clockwise: EncoderBinding::VolumeUp,
            counter_clockwise: EncoderBinding::VolumeDown,
            press_action: EncoderPressAction::MuteApplication,
        })
        .collect()
}

/// Действие на поворот крутилки. `Volume` - старое значение: громкость в сторону вращения.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EncoderBinding { #[default] Volume, VolumeUp, VolumeDown, NoAction, Media(MediaAction) }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZoneCfg {
    pub index: u8,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<Metric>,
    /// Оформление метрики на сенсоре (число / кольцо / полоса).
    #[serde(default)]
    pub style: Style,
    /// Привязанное приложение: имя exe в нижнем регистре (`chrome.exe`).
    /// Хранится даже когда приложение закрыто.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Полный путь к exe — чтобы иконка оставалась доступной без сессии.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_path: Option<String>,
    /// Динамический захват: сам выбирает свободный (не скрытый и не занятый) микшер.
    #[serde(default)]
    pub dynamic: bool,
    /// Процент громкости за один шаг именно этого энкодера.
    #[serde(default = "default_encoder_step")]
    pub step_percent: u8,
    /// Что выполняет нажатие энкодера (legacy, мигрирует в EncoderCfg).
    #[serde(default)]
    pub press_action: EncoderPressAction,
    /// Действие при нажатии/тапе по сенсору на устройстве.
    #[serde(default, skip_serializing_if = "TileAction::is_no_action")]
    pub tap_action: TileAction,
    /// Id устройства для метрики заряда на сенсоре.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_device: Option<String>,
    #[serde(default, skip_serializing_if = "TileColors::is_empty")]
    pub colors: TileColors,
}

impl ZoneCfg {
    /// Есть ли на сенсоре что показывать (приложение, динамика или метрика).
    pub fn has_content(&self) -> bool {
        self.metric.is_some() || self.dynamic || self.app.is_some()
    }
}

fn default_encoder_step() -> u8 { 5 }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPressAction {
    #[default]
    MuteApplication,
    NoAction,
    Media(MediaAction),
    Hotkey { key: String, #[serde(default)] ctrl: bool, #[serde(default)] alt: bool, #[serde(default)] shift: bool, #[serde(default)] win: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeyCfg {
    Metric {
        index: u8,
        metric: Metric,
        #[serde(default)]
        style: Style,
        /// Собственный интервал обновления плитки, мс.
        /// Если не задан — используется общий из `[monitor]`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u64>,
        /// Id устройства для `Metric::DeviceBattery` (мышь/геймпад/BT).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        battery_device: Option<String>,
        /// Действие при нажатии на плитку мониторинга.
        #[serde(default, skip_serializing_if = "TileAction::is_no_action")]
        on_press: TileAction,
        #[serde(default, skip_serializing_if = "TileColors::is_empty")]
        colors: TileColors,
    },
    NextPage {
        index: u8,
        #[serde(default, skip_serializing_if = "TileColors::is_empty")]
        colors: TileColors,
    },
    Launch {
        index: u8,
        path: String,
        #[serde(default, skip_serializing_if = "TileColors::is_empty")]
        colors: TileColors,
    },
    Hotkey {
        index: u8,
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        win: bool,
        #[serde(default, skip_serializing_if = "TileColors::is_empty")]
        colors: TileColors,
    },
    Media {
        index: u8,
        action: MediaAction,
        #[serde(default, skip_serializing_if = "TileColors::is_empty")]
        colors: TileColors,
    },
}

impl KeyCfg {
    pub fn index(&self) -> u8 {
        match self {
            KeyCfg::Metric { index, .. }
            | KeyCfg::NextPage { index, .. }
            | KeyCfg::Launch { index, .. }
            | KeyCfg::Hotkey { index, .. }
            | KeyCfg::Media { index, .. } => *index,
        }
    }

    pub fn set_index(&mut self, value: u8) {
        match self {
            KeyCfg::Metric { index, .. } | KeyCfg::NextPage { index, .. } |
            KeyCfg::Launch { index, .. } | KeyCfg::Hotkey { index, .. } |
            KeyCfg::Media { index, .. } => *index = value,
        }
    }

    pub fn colors(&self) -> &TileColors {
        match self {
            KeyCfg::Metric { colors, .. } | KeyCfg::NextPage { colors, .. } |
            KeyCfg::Launch { colors, .. } | KeyCfg::Hotkey { colors, .. } |
            KeyCfg::Media { colors, .. } => colors,
        }
    }

    pub fn colors_mut(&mut self) -> &mut TileColors {
        match self {
            KeyCfg::Metric { colors, .. } | KeyCfg::NextPage { colors, .. } |
            KeyCfg::Launch { colors, .. } | KeyCfg::Hotkey { colors, .. } |
            KeyCfg::Media { colors, .. } => colors,
        }
    }
}

/// Действие, которое можно повесить на свайп, тап сенсора или плитку метрики.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TileAction {
    #[default]
    NoAction,
    NextPage,
    PrevPage,
    Media(MediaAction),
    Hotkey {
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        win: bool,
    },
    /// Путь к exe, URL или команда вроде `taskmgr`.
    Launch {
        path: String,
    },
}

impl TileAction {
    pub fn is_no_action(&self) -> bool {
        matches!(self, Self::NoAction)
    }

    pub fn label(&self) -> String {
        match self {
            Self::NoAction => "Не назначено".into(),
            Self::NextPage => "Следующая страница".into(),
            Self::PrevPage => "Предыдущая страница".into(),
            Self::Media(action) => action.label().into(),
            Self::Hotkey { key, ctrl, alt, shift, win } => {
                let mut parts = Vec::new();
                if *ctrl {
                    parts.push("Ctrl");
                }
                if *alt {
                    parts.push("Alt");
                }
                if *shift {
                    parts.push("Shift");
                }
                if *win {
                    parts.push("Win");
                }
                if key.trim().is_empty() {
                    parts.push("…");
                } else {
                    parts.push(key.trim());
                }
                format!("Клавиша · {}", parts.join("+"))
            }
            Self::Launch { path } => {
                let path = path.trim();
                if path.is_empty() {
                    "Открыть…".into()
                } else if path.eq_ignore_ascii_case("taskmgr") || path.eq_ignore_ascii_case("taskmgr.exe") {
                    "Диспетчер задач".into()
                } else {
                    format!("Открыть · {path}")
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    VolumeMute,
}

impl MediaAction {
    pub const ALL: [MediaAction; 6] = [
        MediaAction::PlayPause,
        MediaAction::Next,
        MediaAction::Previous,
        MediaAction::VolumeUp,
        MediaAction::VolumeDown,
        MediaAction::VolumeMute,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MediaAction::PlayPause => "Воспроизведение / пауза",
            MediaAction::Next => "Следующий трек",
            MediaAction::Previous => "Предыдущий трек",
            MediaAction::VolumeUp => "Громкость +",
            MediaAction::VolumeDown => "Громкость −",
            MediaAction::VolumeMute => "Выключить звук",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    #[default]
    Value,
    Ring,
    Bar,
}

impl Style {
    pub const ALL: [Style; 3] = [Style::Value, Style::Ring, Style::Bar];

    pub fn label(self) -> &'static str {
        match self {
            Style::Value => "число",
            Style::Ring => "кольцо",
            Style::Bar => "полоса",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    CpuTemp,
    CpuLoad,
    CpuFreq,
    CpuMaxCore,
    CpuThreads,
    RamPercent,
    RamUsed,
    RamTotal,
    GpuTemp,
    GpuLoad,
    GpuVram,
    GpuVramPercent,
    GpuFan,
    DiskUsed,
    DiskActivity,
    DiskUsedGb,
    NetDown,
    NetUp,
    Clock,
    Uptime,
    /// Заряд мыши / геймпада / Bluetooth-устройства.
    DeviceBattery,
}

impl Metric {
    pub const ALL: [Metric; 21] = [
        Metric::CpuTemp,
        Metric::CpuLoad,
        Metric::CpuFreq,
        Metric::CpuMaxCore,
        Metric::CpuThreads,
        Metric::RamPercent,
        Metric::RamUsed,
        Metric::RamTotal,
        Metric::GpuTemp,
        Metric::GpuLoad,
        Metric::GpuVram,
        Metric::GpuVramPercent,
        Metric::GpuFan,
        Metric::DiskUsed,
        Metric::DiskActivity,
        Metric::DiskUsedGb,
        Metric::NetDown,
        Metric::NetUp,
        Metric::Clock,
        Metric::Uptime,
        Metric::DeviceBattery,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Metric::CpuTemp => "Температура CPU",
            Metric::CpuLoad => "Загрузка CPU",
            Metric::CpuFreq => "Частота CPU",
            Metric::CpuMaxCore => "Макс. ядро CPU",
            Metric::CpuThreads => "Потоки CPU",
            Metric::RamPercent => "Память, %",
            Metric::RamUsed => "Память, ГБ",
            Metric::RamTotal => "Всего памяти, ГБ",
            Metric::GpuTemp => "Температура GPU",
            Metric::GpuLoad => "Загрузка GPU",
            Metric::GpuVram => "Видеопамять",
            Metric::GpuVramPercent => "Видеопамять, %",
            Metric::GpuFan => "Вентилятор GPU",
            Metric::DiskUsed => "Диск, %",
            Metric::DiskActivity => "Активность диска, %",
            Metric::DiskUsedGb => "Занято на диске, ГБ",
            Metric::NetDown => "Сеть ↓",
            Metric::NetUp => "Сеть ↑",
            Metric::Clock => "Часы",
            Metric::Uptime => "Аптайм",
            Metric::DeviceBattery => "Заряд устройства",
        }
    }

    /// Метрики со значением в процентах или градусах, к которым применим градиент.
    pub fn supports_gradient(self) -> bool {
        matches!(
            self,
            Metric::CpuTemp
                | Metric::CpuLoad
                | Metric::CpuMaxCore
                | Metric::RamPercent
                | Metric::GpuTemp
                | Metric::GpuLoad
                | Metric::GpuVramPercent
                | Metric::GpuFan
                | Metric::DiskUsed
                | Metric::DiskActivity
                | Metric::DeviceBattery
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        let metric = |index, metric, style| KeyCfg::Metric {
            index,
            metric,
            style,
            interval_ms: None,
            battery_device: None,
            on_press: TileAction::NoAction,
            colors: TileColors::default(),
        };
        let keys = vec![
            metric(1, Metric::CpuTemp, Style::Ring),
            metric(2, Metric::CpuLoad, Style::Bar),
            metric(3, Metric::RamPercent, Style::Bar),
            metric(4, Metric::GpuTemp, Style::Ring),
            metric(5, Metric::GpuLoad, Style::Bar),
            metric(6, Metric::GpuVram, Style::Value),
            metric(7, Metric::NetDown, Style::Value),
            metric(8, Metric::DiskUsed, Style::Bar),
            metric(9, Metric::Uptime, Style::Value),
            metric(10, Metric::Clock, Style::Value),
        ];

        Self {
            version: 1,
            device: DeviceCfg::default(),
            monitor: MonitorCfg::default(),
            audio: AudioCfg::default(),
            app: AppCfg::default(),
            scenes: vec![Scene {
                name: "Основная".into(),
                pages: vec![Page {
                    name: "Страница 1".into(),
                    keys,
                    zones: Vec::new(),
                    encoders: default_encoders(),
                    ..Page::default()
                }],
            }],
            active_scene: 0,
            pages: Vec::new(),
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let neu = base.join("akp-deck").join("config.toml");
    if neu.is_file() {
        return neu;
    }
    // Миграция со старого имени папки.
    let legacy = base.join("ajazz-deck").join("config.toml");
    if legacy.is_file() {
        if let Some(parent) = neu.parent() {
            let _ = std::fs::create_dir_all(parent);
            if std::fs::copy(&legacy, &neu).is_ok() {
                return neu;
            }
        }
        return legacy;
    }
    neu
}

/// Загружает конфиг. При первом запуске создаёт файл с дефолтом.
/// Нечитаемый файл не перезаписывается, повреждённый копируется в config.toml.broken.
/// Вторым значением возвращает текст ошибки для показа в окне.
pub fn load() -> (Config, Option<String>) {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(mut cfg) => {
                cfg.migrate();
                (cfg, None)
            }
            Err(e) => {
                eprintln!("[config] ошибка разбора {}: {e}", path.display());
                let backup = path.with_extension("toml.broken");
                let _ = std::fs::copy(&path, &backup);
                let message = format!(
                    "Ошибка в config.toml, загружены настройки по умолчанию. Копия файла: {}. {e}",
                    backup.display()
                );
                (Config::default(), Some(message))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = Config::default();
            save(&cfg);
            (cfg, None)
        }
        Err(e) => {
            eprintln!("[config] ошибка чтения {}: {e}", path.display());
            let message = format!("Не удалось прочитать config.toml, загружены настройки по умолчанию: {e}");
            (Config::default(), Some(message))
        }
    }
}

/// Перечитывает конфиг после ручной правки файла. При ошибке возвращает её текст,
/// чтобы текущая конфигурация не заменялась дефолтом.
pub fn reload() -> Result<Config, String> {
    let path = config_path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        eprintln!("[config] ошибка чтения {}: {e}", path.display());
        format!("Не удалось прочитать config.toml, изменения не применены: {e}")
    })?;
    match toml::from_str::<Config>(&text) {
        Ok(mut cfg) => {
            cfg.migrate();
            Ok(cfg)
        }
        Err(e) => {
            eprintln!("[config] ошибка разбора {}: {e}", path.display());
            Err(format!("Ошибка в config.toml, изменения не применены: {e}"))
        }
    }
}

pub fn save(cfg: &Config) {
    if let Err(e) = try_save(cfg) {
        eprintln!("[config] ошибка записи {}: {e}", config_path().display());
    }
}

/// Записывает конфигурацию через временный файл, чтобы сбой записи не
/// оставил основной config.toml пустым или частично записанным.
pub fn try_save(cfg: &Config) -> std::io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(cfg)
        .map_err(|e| std::io::Error::other(format!("ошибка сериализации TOML: {e}")))?;
    let temp = path.with_extension("toml.tmp");
    {
        let mut file = std::fs::File::create(&temp)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_sensor_can_be_bound_to_only_one_encoder() {
        let mut page = Page::default();
        page.bind_encoder_sensor(1, Some(2));
        page.bind_encoder_sensor(3, Some(2));
        assert_eq!(page.encoders.iter().find(|e| e.index == 1).unwrap().sensor_index, None);
        assert_eq!(page.encoders.iter().find(|e| e.index == 3).unwrap().sensor_index, Some(2));
    }

    #[test]
    fn duplicate_links_in_old_configs_are_normalized() {
        let mut page = Page::default();
        page.encoders[0].sensor_index = Some(4);
        page.encoders[1].sensor_index = Some(4);
        page.normalize_encoder_sensor_links();
        assert_eq!(page.encoders[0].sensor_index, Some(4));
        assert_eq!(page.encoders[1].sensor_index, None);
    }

    #[test]
    fn digit_color_defaults_and_survives_toml_round_trip() {
        let config = Config::default();
        assert_eq!(config.device.digit_color, [74, 158, 255]);
        let encoded = toml::to_string(&config).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.device.digit_color, config.device.digit_color);
    }

    #[test]
    fn gradients_and_tile_colors_survive_toml_round_trip() {
        let mut config = Config::default();
        config.monitor.gradients.retain(|item| item.metric == Metric::CpuTemp);
        let colors = config.scenes[0].pages[0].keys[0].colors_mut();
        colors.digit = Some([1, 2, 3]);
        colors.gradient = Some(Gradient { green: 10.0, orange: 20.0, red: 30.0 });
        let encoded = toml::to_string(&config).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.monitor.gradients, config.monitor.gradients);
        assert_eq!(decoded.scenes[0].pages[0].keys[0].colors(), config.scenes[0].pages[0].keys[0].colors());
        assert!(decoded.scenes[0].pages[0].keys[1].colors().is_empty());
    }

    #[test]
    fn missing_monitor_gradients_use_defaults() {
        let decoded: MonitorCfg = toml::from_str("interval_ms = 1000").unwrap();
        assert_eq!(decoded.gradient(Metric::CpuTemp), Some(Gradient { green: 30.0, orange: 60.0, red: 80.0 }));
        assert_eq!(decoded.gradient(Metric::Clock), None);
    }

    #[test]
    fn migration_always_leaves_a_page_and_caps_page_count() {
        let mut config = Config::default();
        config.scenes[0].pages.clear();
        config.scenes[0].pages.resize_with(10, Page::default);
        config.migrate();
        assert!(!config.active_pages().is_empty());
        assert_eq!(config.active_pages().len(), 8);
    }

    #[test]
    fn dynamic_capture_picks_distinct_visible_mixers() {
        let mut config = Config::default();
        config.audio.mixers = vec![
            MixerEntry { app: "a.exe".into(), name: "A".into(), exe_path: String::new(), hidden: false },
            MixerEntry { app: "b.exe".into(), name: "B".into(), exe_path: String::new(), hidden: true },
            MixerEntry { app: "c.exe".into(), name: "C".into(), exe_path: String::new(), hidden: false },
        ];
        config.scenes[0].pages[0].zones = vec![
            ZoneCfg {
                index: 1, title: "Динамический".into(), metric: None, style: Style::Value,
                app: None, app_path: None, dynamic: true, step_percent: 5,
                press_action: EncoderPressAction::MuteApplication,
                tap_action: TileAction::NoAction, battery_device: None, colors: TileColors::default(),
            },
            ZoneCfg {
                index: 2, title: "Динамический".into(), metric: None, style: Style::Value,
                app: None, app_path: None, dynamic: true, step_percent: 5,
                press_action: EncoderPressAction::MuteApplication,
                tap_action: TileAction::NoAction, battery_device: None, colors: TileColors::default(),
            },
            ZoneCfg {
                index: 3, title: "Fixed".into(), metric: None, style: Style::Value,
                app: Some("c.exe".into()), app_path: None, dynamic: false, step_percent: 5,
                press_action: EncoderPressAction::MuteApplication,
                tap_action: TileAction::NoAction, battery_device: None, colors: TileColors::default(),
            },
        ];
        let sessions = [("a.exe", "A", ""), ("b.exe", "B", ""), ("c.exe", "C", ""), ("d.exe", "D", "")];
        assert!(config.resolve_dynamic_apps(sessions));
        let zones = &config.scenes[0].pages[0].zones;
        assert_eq!(zones[0].app.as_deref(), Some("a.exe"));
        assert_eq!(zones[1].app.as_deref(), Some("d.exe"));
        assert_eq!(zones[2].app.as_deref(), Some("c.exe"));
    }

    #[test]
    fn encoder_links_follow_sensor_move() {
        let mut page = Page::default();
        page.encoders[0].sensor_index = Some(1);
        page.encoders[1].sensor_index = Some(2);
        page.move_encoder_links(1, 3, false);
        assert_eq!(page.encoders[0].sensor_index, Some(3));
        assert_eq!(page.encoders[1].sensor_index, Some(2));
    }

    #[test]
    fn encoder_links_swap_with_sensors() {
        let mut page = Page::default();
        page.encoders[0].sensor_index = Some(1);
        page.encoders[1].sensor_index = Some(2);
        page.move_encoder_links(1, 2, true);
        assert_eq!(page.encoders[0].sensor_index, Some(2));
        assert_eq!(page.encoders[1].sensor_index, Some(1));
    }

    #[test]
    fn auto_bind_prefers_matching_free_encoder() {
        let mut page = Page::default();
        assert!(page.auto_bind_free_encoder(3));
        assert_eq!(page.encoders[2].sensor_index, Some(3));
        assert!(!page.auto_bind_free_encoder(3));
    }
}

pub fn mtime() -> Option<SystemTime> {
    std::fs::metadata(config_path())
        .and_then(|m| m.modified())
        .ok()
}
