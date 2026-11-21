//! Событийные действия кнопок. Здесь нет фонового опроса или перехвата ввода.

use anyhow::{anyhow, Result};
use crate::config::{MediaAction, TileAction};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, KEYBDINPUT, KEYBD_EVENT_FLAGS, INPUT_KEYBOARD, KEYEVENTF_KEYUP,
    KEYEVENTF_EXTENDEDKEY, VIRTUAL_KEY,
};

/// Результат выполнения действия: смена страницы или только side-effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionEffect {
    None,
    NextPage,
    PrevPage,
}

/// Запускает путь / URL / `taskmgr` через обработчик Windows без окна консоли.
pub fn launch_path(path: &str) -> Result<()> {
    let path = path.trim();
    if path.is_empty() {
        return Err(anyhow!("пустой путь запуска"));
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("explorer.exe")
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("не удалось открыть {path}: {e}"))
}

/// Выполняет общее действие плитки / свайпа / тапа.
pub fn run_tile_action(action: &TileAction) -> Result<ActionEffect> {
    match action {
        TileAction::NoAction => Ok(ActionEffect::None),
        TileAction::NextPage => Ok(ActionEffect::NextPage),
        TileAction::PrevPage => Ok(ActionEffect::PrevPage),
        TileAction::Media(media) => {
            send_media(*media)?;
            Ok(ActionEffect::None)
        }
        TileAction::Hotkey {
            key,
            ctrl,
            alt,
            shift,
            win,
        } => {
            if key.trim().is_empty() {
                return Ok(ActionEffect::None);
            }
            send_hotkey(key.trim(), *ctrl, *alt, *shift, *win)?;
            Ok(ActionEffect::None)
        }
        TileAction::Launch { path } => {
            launch_path(path)?;
            Ok(ActionEffect::None)
        }
    }
}

fn virtual_key(key: &str) -> Option<u16> {
    let normalized = key.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "SPACE" => Some(0x20),
        "ENTER" => Some(0x0D),
        "TAB" => Some(0x09),
        "ESC" | "ESCAPE" => Some(0x1B),
        "UP" => Some(0x26),
        "DOWN" => Some(0x28),
        "LEFT" => Some(0x25),
        "RIGHT" => Some(0x27),
        value if value.len() == 1 && value.as_bytes()[0].is_ascii_alphanumeric() => {
            Some(value.as_bytes()[0] as u16)
        }
        value if value.starts_with('F') => value[1..]
            .parse::<u16>()
            .ok()
            .filter(|n| (1..=24).contains(n))
            .map(|n| 0x70 + n - 1),
        _ => None,
    }
}

/// Отправляет обычное нажатие/отпускание сочетания клавиш Windows.
pub fn send_hotkey(key: &str, ctrl: bool, alt: bool, shift: bool, win: bool) -> Result<()> {
    let key = virtual_key(key).ok_or_else(|| anyhow!("неизвестная клавиша сочетания"))?;
    let mut modifiers = Vec::with_capacity(3);
    if ctrl {
        modifiers.push(0x11);
    }
    if alt {
        modifiers.push(0x12);
    }
    if shift {
        modifiers.push(0x10);
    }
    if win {
        modifiers.push(0x5B);
    }

    let mut inputs = Vec::with_capacity(modifiers.len() * 2 + 2);
    for &vk in &modifiers {
        inputs.push(key_input(vk, false));
    }
    inputs.push(key_input(key, false));
    inputs.push(key_input(key, true));
    for &vk in modifiers.iter().rev() {
        inputs.push(key_input(vk, true));
    }

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(anyhow!("Windows отправил {sent} из {} событий клавиатуры", inputs.len()));
    }
    Ok(())
}

pub fn send_media(action: MediaAction) -> Result<()> {
    let vk = match action {
        MediaAction::PlayPause => 0xB3,
        MediaAction::Next => 0xB0,
        MediaAction::Previous => 0xB1,
        MediaAction::VolumeUp => 0xAF,
        MediaAction::VolumeDown => 0xAE,
        MediaAction::VolumeMute => 0xAD,
    };
    let inputs = [media_input(vk, false), media_input(vk, true)];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(anyhow!("Windows отправил {sent} из {} событий мультимедиа", inputs.len()));
    }
    Ok(())
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn media_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: KEYEVENTF_EXTENDEDKEY
                    | if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
