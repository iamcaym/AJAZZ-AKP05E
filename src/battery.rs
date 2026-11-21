//! Заряд беспроводных устройств.
//!
//! Источники (по приоритету в списке):
//! 1. HID Aurora/Sinowealth feature `0x83` — Lamzu и ряд мышей на том же протоколе
//! 2. DualSense / DualSense Edge (USB и Bluetooth HID)
//! 3. Windows `DEVPKEY_Bluetooth_Battery` — любые BT-устройства с GATT Battery Service

use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use hidapi::{DeviceInfo, HidApi};

/// Известные VID:PID с протоколом Aurora feature 0x83 (интерфейс 2, usage 0xFFFF).
const AURORA_DEVICES: &[(u16, u16)] = &[
    (0x373E, 0x001E), // Lamzu Maya X dongle
    (0x373E, 0x001C), // Lamzu Maya X wired
];

/// Подсказки в имени продукта для того же протокола без нового PID в таблице.
const AURORA_NAME_HINTS: &[&str] = &[
    "lamzu",
    "maya",
    "attack shark",
    "wlmouse",
    "g-wolves",
    "glorious model o",
    "glorious model d",
];

const SONY_VID: u16 = 0x054C;
const DUALSENSE_PID: u16 = 0x0CE6;
const DUALSENSE_EDGE_PID: u16 = 0x0DF2;

const CACHE_TTL: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryKind {
    Mouse,
    Controller,
    Bluetooth,
}

impl BatteryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mouse => "Мышь / донгл",
            Self::Controller => "Геймпад",
            Self::Bluetooth => "Bluetooth",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BatteryDevice {
    pub id: String,
    pub name: String,
    pub kind: BatteryKind,
    pub percent: Option<u8>,
    pub charging: bool,
    pub connected: bool,
}

struct Cache {
    at: Instant,
    devices: Vec<BatteryDevice>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Список устройств с актуальным зарядом (кэш ~8 с).
pub fn list_devices() -> Vec<BatteryDevice> {
    if let Ok(guard) = CACHE.lock() {
        if let Some(cache) = guard.as_ref() {
            if cache.at.elapsed() < CACHE_TTL {
                return cache.devices.clone();
            }
        }
    }
    let devices = scan_now();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(Cache {
            at: Instant::now(),
            devices: devices.clone(),
        });
    }
    devices
}

/// Заряд выбранного устройства.
pub fn read_device(id: &str) -> Option<BatteryDevice> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    list_devices().into_iter().find(|d| d.id == id)
}

fn scan_now() -> Vec<BatteryDevice> {
    let mut out = Vec::new();

    if let Ok(api) = HidApi::new() {
        for dev in read_aurora_mice(&api) {
            out.push(dev);
        }
        for dev in read_dualsense(&api) {
            if !out.iter().any(|d| d.id == dev.id) {
                out.push(dev);
            }
        }
    }

    // Стандартный заряд Windows для BT-устройств с GATT Battery Service
    // (наушники, клавиатуры, мыши). DualSense сюда обычно не попадает.
    for dev in read_bluetooth_batteries() {
        let name_l = dev.name.to_ascii_lowercase();
        let dup_hid = name_l.contains("dualsense")
            || name_l.contains("wireless controller")
            || out.iter().any(|d| d.name.eq_ignore_ascii_case(&dev.name));
        if !dup_hid {
            out.push(dev);
        }
    }
    out
}

fn is_aurora_candidate(info: &DeviceInfo) -> bool {
    if info.usage_page() != 0xFFFF || info.interface_number() != 2 {
        return false;
    }
    let vid = info.vendor_id();
    let pid = info.product_id();
    if AURORA_DEVICES.iter().any(|&(v, p)| v == vid && p == pid) {
        return true;
    }
    let name = product_name(info)
        .unwrap_or_default()
        .to_ascii_lowercase();
    AURORA_NAME_HINTS.iter().any(|h| name.contains(h))
}

fn read_aurora_mice(api: &HidApi) -> Vec<BatteryDevice> {
    let mut out = Vec::new();
    let mut seen = Vec::new();

    for info in api.device_list().filter(|d| is_aurora_candidate(d)) {
        let key = (info.vendor_id(), info.product_id(), info.interface_number());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);

        let via_usb = info.product_id() == 0x001C
            || product_name(info)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("wired");
        let connection = if via_usb { "usb" } else { "dongle" };
        // Префикс `lamzu:` сохраняем для совместимости со старыми config.toml.
        let prefix = if info.vendor_id() == 0x373E {
            "lamzu"
        } else {
            "aurora"
        };
        let id = format!(
            "{prefix}:{:04x}:{:04x}:{connection}",
            info.vendor_id(),
            info.product_id()
        );
        let name = product_name(info).unwrap_or_else(|| {
            if info.vendor_id() == 0x373E {
                if via_usb {
                    "Lamzu Maya X (USB)".into()
                } else {
                    "Lamzu Maya X".into()
                }
            } else {
                format!(
                    "Мышь {:04X}:{:04X} ({connection})",
                    info.vendor_id(),
                    info.product_id()
                )
            }
        });

        let reading = query_aurora(api, info);
        let (percent, charging) = match reading {
            Some((pct, ch)) => (Some(pct.min(100)), ch),
            None => (None, false),
        };

        out.push(BatteryDevice {
            id,
            name,
            kind: BatteryKind::Mouse,
            percent,
            charging,
            connected: true,
        });
    }
    out
}

fn query_aurora(api: &HidApi, info: &DeviceInfo) -> Option<(u8, bool)> {
    let device = info.open_device(api).ok()?;
    // Feature report: report id 0 + `00 00 02 02 00 83 …`
    let mut request = [0u8; 65];
    request[0] = 0x00;
    request[3] = 0x02;
    request[4] = 0x02;
    request[6] = 0x83;
    device.send_feature_report(&request).ok()?;
    thread::sleep(Duration::from_millis(120));
    let mut response = [0u8; 65];
    response[0] = 0x00;
    device.get_feature_report(&mut response).ok()?;
    if response[1] == 0xA1 && response[6] == 0x83 {
        Some((response[8], response[7] == 1))
    } else if response[1] == 0xA1 && response.len() > 8 {
        let pct = response[8];
        if pct <= 100 {
            Some((pct, response[7] == 1))
        } else {
            None
        }
    } else {
        None
    }
}

fn is_dualsense(info: &DeviceInfo) -> bool {
    if info.vendor_id() != SONY_VID {
        return false;
    }
    let pid = info.product_id();
    if pid == DUALSENSE_PID || pid == DUALSENSE_EDGE_PID {
        return true;
    }
    let name = product_name(info)
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.contains("dualsense") || name.contains("wireless controller")
}

fn read_dualsense(api: &HidApi) -> Vec<BatteryDevice> {
    let mut out = Vec::new();
    for info in api.device_list().filter(|d| is_dualsense(d)) {
        let path = info.path().to_string_lossy().to_lowercase();
        let via_bt = path.contains("bth") || path.contains("bluetooth");
        let edge = info.product_id() == DUALSENSE_EDGE_PID
            || product_name(info)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("edge");
        let id = format!(
            "dualsense:{:04x}:{:04x}:{}",
            info.vendor_id(),
            info.product_id(),
            if via_bt { "bt" } else { "usb" }
        );
        if out.iter().any(|d: &BatteryDevice| d.id == id) {
            continue;
        }
        let name = product_name(info).unwrap_or_else(|| {
            if edge {
                if via_bt {
                    "DualSense Edge (Bluetooth)".into()
                } else {
                    "DualSense Edge".into()
                }
            } else if via_bt {
                "DualSense (Bluetooth)".into()
            } else {
                "DualSense".into()
            }
        });

        let reading = query_dualsense(api, info, via_bt);
        if reading.is_none() {
            let usage = info.usage();
            let page = info.usage_page();
            let looks_gamepad = page == 0x01 && (usage == 0x05 || usage == 0x04);
            if !looks_gamepad && info.interface_number() != 3 {
                continue;
            }
        }

        let (percent, charging) = reading.unwrap_or((None, false));
        out.push(BatteryDevice {
            id,
            name,
            kind: BatteryKind::Controller,
            percent,
            charging,
            connected: true,
        });
    }
    out
}

fn query_dualsense(api: &HidApi, info: &DeviceInfo, via_bt: bool) -> Option<(Option<u8>, bool)> {
    let device = info.open_device(api).ok()?;
    device.set_blocking_mode(false).ok()?;
    let mut buf = [0u8; 128];
    for _ in 0..12 {
        match device.read_timeout(&mut buf, 40) {
            Ok(n) if n > 0 => {
                if let Some((pct, charging)) = parse_dualsense_report(&buf[..n], via_bt) {
                    return Some((Some(pct), charging));
                }
            }
            _ => {}
        }
    }
    if !via_bt {
        let mut report = [0u8; 64];
        report[0] = 0x01;
        if device.get_input_report(&mut report).is_ok() {
            if let Some((pct, charging)) = parse_dualsense_report(&report, false) {
                return Some((Some(pct), charging));
            }
        }
    }
    None
}

fn parse_dualsense_report(buf: &[u8], prefer_bt: bool) -> Option<(u8, bool)> {
    if buf.is_empty() {
        return None;
    }
    let (offset, ok) = match buf[0] {
        0x01 if buf.len() > 53 => (53usize, true),
        0x31 if buf.len() > 54 => (54usize, true),
        _ if prefer_bt && buf.len() > 54 => (54, true),
        _ if !prefer_bt && buf.len() > 53 => (53, buf.len() >= 64),
        _ => return None,
    };
    if !ok {
        return None;
    }
    let b = *buf.get(offset)?;
    let level = b & 0x0F;
    let status = b >> 4;
    let percent = match status {
        0x2 => 100,
        _ => (level.saturating_mul(10).saturating_add(5)).min(100),
    };
    let charging = matches!(status, 0x1 | 0x2);
    if level > 10 && status != 0x2 {
        return None;
    }
    Some((percent, charging))
}

fn product_name(info: &DeviceInfo) -> Option<String> {
    info.product_string()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// DEVPKEY_Bluetooth_Battery — процент 0..=100 у устройств с GATT Battery.
fn read_bluetooth_batteries() -> Vec<BatteryDevice> {
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_Get_DevNode_PropertyW,
        CM_Locate_DevNodeW, CM_GETIDLIST_FILTER_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CONFIGRET,
    };
    use windows::Win32::Devices::Properties::{
        DEVPROPTYPE, DEVPROP_TYPE_BYTE, DEVPROP_TYPE_STRING, DEVPROP_TYPE_UINT32,
    };
    use windows::Win32::Foundation::DEVPROPKEY;

    const DEVPKEY_BLUETOOTH_BATTERY: DEVPROPKEY = DEVPROPKEY {
        fmtid: GUID::from_u128(0x104e_a319_6ee2_4701_bd47_8ddb_f425_bbe5),
        pid: 2,
    };
    const DEVPKEY_NAME: DEVPROPKEY = DEVPROPKEY {
        fmtid: GUID::from_u128(0xb725_f130_47ef_101a_a5f1_0260_8c9e_eb8c),
        pid: 10,
    };

    unsafe {
        let mut chars = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut chars, PCWSTR::null(), CM_GETIDLIST_FILTER_PRESENT)
            != CONFIGRET(0)
            || chars == 0
        {
            return Vec::new();
        }
        let mut buf = vec![0u16; chars as usize];
        if CM_Get_Device_ID_ListW(PCWSTR::null(), &mut buf, CM_GETIDLIST_FILTER_PRESENT)
            != CONFIGRET(0)
        {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut start = 0usize;
        while start < buf.len() {
            if buf[start] == 0 {
                break;
            }
            let end = buf[start..]
                .iter()
                .position(|&c| c == 0)
                .map(|i| start + i)
                .unwrap_or(buf.len());
            let id_wide = &buf[start..end];
            start = end + 1;

            let id_str = String::from_utf16_lossy(id_wide);
            let id_up = id_str.to_ascii_uppercase();
            if !(id_up.contains("BTHENUM") || id_up.contains("BTHLE") || id_up.contains("BTH\\")) {
                continue;
            }

            let mut id_z = id_wide.to_vec();
            id_z.push(0);
            let mut devinst = 0u32;
            if CM_Locate_DevNodeW(
                &mut devinst,
                PCWSTR(id_z.as_ptr()),
                CM_LOCATE_DEVNODE_NORMAL,
            ) != CONFIGRET(0)
            {
                continue;
            }

            let mut prop_type = DEVPROPTYPE(0);
            let mut percent_buf = [0u8; 4];
            let mut size = percent_buf.len() as u32;
            let cr = CM_Get_DevNode_PropertyW(
                devinst,
                &DEVPKEY_BLUETOOTH_BATTERY,
                &mut prop_type,
                Some(percent_buf.as_mut_ptr()),
                &mut size,
                0,
            );
            if cr != CONFIGRET(0) {
                continue;
            }
            let percent = if prop_type == DEVPROP_TYPE_BYTE {
                percent_buf[0]
            } else if prop_type == DEVPROP_TYPE_UINT32 {
                u32::from_le_bytes(percent_buf) as u8
            } else {
                continue;
            };
            if percent > 100 {
                continue;
            }

            let mut name_type = DEVPROPTYPE(0);
            let mut name_bytes = vec![0u8; 512];
            let mut name_size = name_bytes.len() as u32;
            let name = if CM_Get_DevNode_PropertyW(
                devinst,
                &DEVPKEY_NAME,
                &mut name_type,
                Some(name_bytes.as_mut_ptr()),
                &mut name_size,
                0,
            ) == CONFIGRET(0)
                && name_type == DEVPROP_TYPE_STRING
                && name_size >= 2
            {
                let u16s: Vec<u16> = name_bytes[..name_size as usize]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                let n = String::from_utf16_lossy(&u16s);
                if n.trim().is_empty() {
                    id_str.clone()
                } else {
                    n
                }
            } else {
                id_str.clone()
            };

            out.push(BatteryDevice {
                id: format!("bt:{id_str}"),
                name,
                kind: BatteryKind::Bluetooth,
                percent: Some(percent),
                charging: false,
                connected: true,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::parse_dualsense_report;

    #[test]
    fn parses_usb_dualsense_battery() {
        let mut buf = [0u8; 64];
        buf[0] = 0x01;
        buf[53] = 0x05; // discharging, level 5 → ~55%
        let (pct, charging) = parse_dualsense_report(&buf, false).unwrap();
        assert_eq!(pct, 55);
        assert!(!charging);
    }

    #[test]
    fn parses_bt_dualsense_full() {
        let mut buf = [0u8; 78];
        buf[0] = 0x31;
        buf[54] = 0x2F; // full
        let (pct, charging) = parse_dualsense_report(&buf, true).unwrap();
        assert_eq!(pct, 100);
        assert!(charging);
    }
}
