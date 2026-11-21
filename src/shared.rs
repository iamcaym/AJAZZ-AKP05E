//! Общее состояние между циклом устройства и окном конфигурации.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use image::RgbaImage;

use crate::config::Config;
use crate::monitor::Snapshot;

const LOG_CAPACITY: usize = 100;

/// Одна активная аудиосессия Windows (микшер).
#[derive(Clone, Debug, Default)]
pub struct SessionInfo {
    /// Ключ приложения: имя exe в нижнем регистре, например `chrome.exe`.
    pub app: String,
    /// Человекочитаемое имя без расширения, например `chrome`.
    pub name: String,
    /// Полный путь к exe — нужен для иконки и для сохранения привязки.
    pub exe_path: String,
    pub volume: i32,
    pub muted: bool,
}

/// Состояние аудиомикшера, которое читает и окно, и цикл устройства.
#[derive(Clone, Debug, Default)]
pub struct AudioState {
    pub sessions: Vec<SessionInfo>,
    /// Кэш иконок по ключу приложения (`chrome.exe`).
    pub icons: HashMap<String, Arc<RgbaImage>>,
    pub error: Option<String>,
    pub ready: bool,
}

impl AudioState {
    pub fn session(&self, app: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.app == app)
    }

    pub fn icon(&self, app: &str) -> Option<&RgbaImage> {
        self.icons.get(app).map(|icon| icon.as_ref())
    }
}

pub struct Shared {
    /// Последний применённый конфиг.
    pub config: Config,
    /// Активная страница устройства (индекс в `config.pages`).
    pub active_page: usize,
    /// Активная сцена устройства (индекс в `config.scenes`).
    pub active_scene: usize,
    /// Инкрементируется UI при сохранении новой сцены или страницы.
    pub config_revision: u64,
    /// Последний снимок метрик.
    pub snapshot: Option<Snapshot>,
    /// Статус устройства для строки состояния в окне.
    pub status: String,
    /// Текущие значения крутилок 1..4.
    pub volumes: [i32; 4],
    /// Заглушена ли каждая крутилка 1..4 (нажатие переключает mute).
    pub muted: [bool; 4],
    /// Живое состояние микшера Windows. Читатели берут копию `Arc`, а не всего состояния.
    pub audio: Arc<Mutex<Arc<AudioState>>>,
    /// Ошибка чтения или разбора config.toml, пока она не исправлена.
    pub config_error: Option<String>,
    /// Журнал событий (последние `LOG_CAPACITY` записей).
    pub log: VecDeque<String>,
    /// Закрытие окна прячет его в трей, а не завершает приложение.
    pub hide_to_tray: bool,
    /// Трей просит показать окно. Окно сбрасывает флаг.
    pub tray_show: bool,
    /// Трей просит завершить приложение. Окно сбрасывает флаг.
    pub tray_quit: bool,
    /// Контекст окна: трей будит через него скрытое окно.
    pub egui_ctx: Option<eframe::egui::Context>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            config: Config::default(),
            active_page: 0,
            active_scene: 0,
            config_revision: 0,
            snapshot: None,
            status: "запуск…".to_string(),
            volumes: [50; 4],
            muted: [false; 4],
            audio: Arc::new(Mutex::new(Arc::new(AudioState::default()))),
            config_error: None,
            log: VecDeque::new(),
            hide_to_tray: false,
            tray_show: false,
            tray_quit: false,
            egui_ctx: None,
        }
    }
}

impl Shared {
    pub fn push_log(&mut self, message: String) {
        while self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(message);
    }
}
