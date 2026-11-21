//! Подключение к панели AKP05E (mirajazz v3) и разбор входящих пакетов.
//!
//! Карта протокола снята с живой платы (см. CHRONOLOGY.md, раздел 4).
//! Все входные пакеты начинаются с `ACK\0\0OK`; код события — в байте 9,
//! состояние — в байте 10. Пакет-подтверждение загрузки картинки — 7 байт,
//! у него байт 9 равен нулю и отбрасывается.

use anyhow::{anyhow, Result};
use mirajazz::device::{list_devices, Device, DeviceQuery};
use mirajazz::types::{ImageFormat, ImageMirroring, ImageMode, ImageRotation};

pub const VID: u16 = 0x0300;
pub const PID: u16 = 0x3004;
pub const USAGE_PAGE: u16 = 0xFFA0;
pub const USAGE_ID: u16 = 1;
pub const PROTOCOL_VERSION: usize = 3;

pub const KEY_COUNT: usize = 10;
pub const ENCODER_COUNT: usize = 4;
pub const ZONE_COUNT: usize = 4;

/// Картинка клавиши: 112×112 JPEG, поворот 180°.
pub const KEY_FORMAT: ImageFormat = ImageFormat {
    mode: ImageMode::JPEG,
    size: (112, 112),
    rotation: ImageRotation::Rot180,
    mirror: ImageMirroring::None,
};

/// Картинка зоны тач-стрипа: 176×112 JPEG, поворот 180°.
pub const ZONE_FORMAT: ImageFormat = ImageFormat {
    mode: ImageMode::JPEG,
    size: (176, 112),
    rotation: ImageRotation::Rot180,
    mirror: ImageMirroring::None,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeckEvent {
    /// Клавиша 1..10: нажата (`down = true`) или отпущена.
    Key { key: u8, down: bool },
    /// Поворот крутилки 1..4: `delta` — число щелчков, знак — направление.
    Twist { encoder: u8, delta: i8 },
    /// Нажатие на крутилку 1..4 (события отпускания у устройства нет).
    Press { encoder: u8 },
    /// Свайп по тач-стрипу.
    Swipe { left: bool },
    /// Тап по зоне тач-стрипа (1..=4).
    TouchTap { zone: u8 },
}

/// Логическая клавиша 1..10 → индекс картинки.
pub fn wire_index(key: u8) -> Option<u8> {
    match key {
        1..=5 => Some(key + 9),
        6..=10 => Some(key - 1),
        _ => None,
    }
}

/// Зона стрипа 1..4 → индекс картинки.
pub fn zone_index(zone: u8) -> Option<u8> {
    if (1..=ZONE_COUNT as u8).contains(&zone) {
        Some(zone - 1)
    } else {
        None
    }
}

/// Разбирает сырой пакет устройства в событие.
pub fn parse(buf: &[u8]) -> Option<DeckEvent> {
    let tag = *buf.get(9)?;
    let state = *buf.get(10)?;

    match tag {
        0x01..=0x0A => Some(DeckEvent::Key {
            key: tag,
            down: state != 0,
        }),
        0xA0 => Some(DeckEvent::Twist {
            encoder: 1,
            delta: -1,
        }),
        0xA1 => Some(DeckEvent::Twist {
            encoder: 1,
            delta: 1,
        }),
        0x50 => Some(DeckEvent::Twist {
            encoder: 2,
            delta: -1,
        }),
        0x51 => Some(DeckEvent::Twist {
            encoder: 2,
            delta: 1,
        }),
        0x90 => Some(DeckEvent::Twist {
            encoder: 3,
            delta: -1,
        }),
        0x91 => Some(DeckEvent::Twist {
            encoder: 3,
            delta: 1,
        }),
        0x70 => Some(DeckEvent::Twist {
            encoder: 4,
            delta: -1,
        }),
        0x71 => Some(DeckEvent::Twist {
            encoder: 4,
            delta: 1,
        }),
        0x37 => Some(DeckEvent::Press { encoder: 1 }),
        0x35 => Some(DeckEvent::Press { encoder: 2 }),
        0x33 => Some(DeckEvent::Press { encoder: 3 }),
        0x36 => Some(DeckEvent::Press { encoder: 4 }),
        // На AKP05E байты свайпа зеркальны относительно «физического» направления.
        0x38 => Some(DeckEvent::Swipe { left: false }),
        0x39 => Some(DeckEvent::Swipe { left: true }),
        // Тап по зонам стрипа (часть прошивок Stream Dock-совместимых панелей).
        0x3A if state != 0 => Some(DeckEvent::TouchTap { zone: 1 }),
        0x3B if state != 0 => Some(DeckEvent::TouchTap { zone: 2 }),
        0x3C if state != 0 => Some(DeckEvent::TouchTap { zone: 3 }),
        0x3D if state != 0 => Some(DeckEvent::TouchTap { zone: 4 }),
        _ => None,
    }
}

/// Краткое описание сырого пакета для отладки неизвестных кодов.
pub fn describe_raw(buf: &[u8]) -> String {
    let tag = buf.get(9).copied().unwrap_or(0);
    let state = buf.get(10).copied().unwrap_or(0);
    format!("tag=0x{tag:02X} state=0x{state:02X}")
}

pub struct Deck {
    pub dev: Device,
    pub serial: String,
}

/// Известные панели на протоколе mirajazz v3 (можно расширять без правок кода
/// через `[device] vid` / `pid` в config.toml или `AKPDECK_VID` / `AKPDECK_PID`).
const KNOWN_DECKS: &[(u16, u16, &str)] = &[
    (VID, PID, "AKP05E"),
];

/// Подключение: сначала `vid`/`pid` из конфига (или env), затем известные модели.
pub async fn connect(vid: u16, pid: u16) -> Result<Deck> {
    let env_vid = parse_hex_u16(
        std::env::var("AKPDECK_VID")
            .or_else(|_| std::env::var("AJAZZ_VID"))
            .ok()
            .as_deref(),
    );
    let env_pid = parse_hex_u16(
        std::env::var("AKPDECK_PID")
            .or_else(|_| std::env::var("AJAZZ_PID"))
            .ok()
            .as_deref(),
    );
    let vid = env_vid.unwrap_or(vid);
    let pid = env_pid.unwrap_or(pid);

    let mut tried = Vec::new();
    let mut candidates = vec![(vid, pid)];
    for &(known_vid, known_pid, _) in KNOWN_DECKS {
        if !candidates
            .iter()
            .any(|&(v, p)| v == known_vid && p == known_pid)
        {
            candidates.push((known_vid, known_pid));
        }
    }

    let mut last_err: Option<anyhow::Error> = None;
    for (try_vid, try_pid) in candidates {
        tried.push(format!("{try_vid:04X}:{try_pid:04X}"));
        match try_connect(try_vid, try_pid).await {
            Ok(deck) => return Ok(deck),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        anyhow!(
            "панель не найдена (пробовали {}). Закройте родное приложение панели \
             (Stream Dock и т.п.), проверьте кабель или задайте [device] vid/pid в config.toml",
            tried.join(", ")
        )
    }))
}

async fn try_connect(vid: u16, pid: u16) -> Result<Deck> {
    let queries = [DeviceQuery::new(USAGE_PAGE, USAGE_ID, vid, pid)];

    let devices = list_devices(&queries)
        .await
        .map_err(|e| anyhow!("поиск устройства {vid:04X}:{pid:04X}: {e}"))?;

    let info = devices.into_iter().next().ok_or_else(|| {
        anyhow!("устройство {vid:04X}:{pid:04X} не найдено")
    })?;

    let info = info.to_device_info();

    let dev = Device::connect(&info, PROTOCOL_VERSION, KEY_COUNT, ENCODER_COUNT)
        .await
        .map_err(|e| anyhow!("подключение к {vid:04X}:{pid:04X}: {e}"))?;

    let serial = dev.serial_number().clone();

    Ok(Deck { dev, serial })
}

fn parse_hex_u16(raw: Option<&str>) -> Option<u16> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let cleaned = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw);
    u16::from_str_radix(cleaned, 16)
        .ok()
        .or_else(|| raw.parse().ok())
}
