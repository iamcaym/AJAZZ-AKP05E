//! Пассивное чтение LibreHardwareMonitor HTTP API.
//!
//! Мы **не** ставим и **не** запускаем LHM. Если пользователь сам держит
//! LHM с веб-сервером на :18185 / :8085 — подхватываем CPU/GPU. Иначе
//! температура CPU идёт через HWiNFO (см. `hwinfo.rs`).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::Deserialize;

const LHM_PORT: u16 = 18185;
const LHM_FALLBACK_PORT: u16 = 8085;

#[derive(Debug, Deserialize)]
struct Node {
    #[serde(default, rename = "Text")]
    text: String,
    #[serde(default, rename = "Type")]
    type_name: String,
    #[serde(default, rename = "Value")]
    value: String,
    #[serde(default, rename = "SensorId")]
    sensor_id: String,
    #[serde(default, rename = "Children")]
    children: Vec<Node>,
}

/// Температура CPU из уже запущенного LHM (без установки/запуска).
pub fn cpu_temp_if_running(select: impl Fn(Vec<(String, f32)>) -> Option<f32>) -> Option<f32> {
    read_from_ports(&select)
}

fn read_from_ports(select: &impl Fn(Vec<(String, f32)>) -> Option<f32>) -> Option<f32> {
    for port in [LHM_PORT, LHM_FALLBACK_PORT] {
        if let Some(body) = http_get_local(port, "/data.json") {
            if let Some(temp) = parse_cpu_temp(&body, select) {
                return Some(temp);
            }
        }
    }
    None
}

fn parse_cpu_temp(body: &str, select: &impl Fn(Vec<(String, f32)>) -> Option<f32>) -> Option<f32> {
    let root: Node = serde_json::from_str(body).ok()?;
    let mut temps = Vec::new();
    collect_temps(&root, &mut temps);
    select(temps)
}

fn collect_temps(node: &Node, out: &mut Vec<(String, f32)>) {
    if node.type_name.eq_ignore_ascii_case("Temperature") {
        if let Some(temp) = parse_temp_value(&node.value) {
            let mut label = node.text.clone();
            if !node.sensor_id.is_empty() {
                label = format!("{} {}", node.sensor_id, label);
            }
            out.push((label, temp));
        }
    }
    for child in &node.children {
        collect_temps(child, out);
    }
}

fn parse_temp_value(raw: &str) -> Option<f32> {
    let cleaned: String = raw
        .chars()
        .map(|c| if c == ',' { '.' } else { c })
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    let temp: f32 = cleaned.parse().ok()?;
    (temp.is_finite() && (0.0..=130.0).contains(&temp)).then_some(temp)
}

fn http_get_local(port: u16, path: &str) -> Option<String> {
    let mut stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(250)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(700)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(300)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1)?;
    if !text.starts_with("HTTP/1.1 200") && !text.starts_with("HTTP/1.0 200") {
        return None;
    }
    Some(body.to_string())
}

/// Снимок GPU из LibreHardwareMonitor (AMD / Intel / NVIDIA без NVML).
#[derive(Clone, Debug, Default)]
pub struct LhmGpu {
    pub name: String,
    pub temp_c: Option<f32>,
    pub load: Option<f32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub fan_percent: Option<u32>,
}

/// GPU из уже запущенного LHM — без установки и запуска окна.
pub fn gpu_if_running(index: usize) -> Option<LhmGpu> {
    read_gpus_from_ports().and_then(|list| list.get(index).cloned())
}

fn read_gpus_from_ports() -> Option<Vec<LhmGpu>> {
    for port in [LHM_PORT, LHM_FALLBACK_PORT] {
        if let Some(body) = http_get_local(port, "/data.json") {
            let list = parse_gpus(&body);
            if !list.is_empty() {
                return Some(list);
            }
        }
    }
    None
}

fn parse_gpus(body: &str) -> Vec<LhmGpu> {
    let Ok(root) = serde_json::from_str::<Node>(body) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_gpus(&root, &mut out);
    out
}

fn collect_gpus(node: &Node, out: &mut Vec<LhmGpu>) {
    let type_l = node.type_name.to_ascii_lowercase();
    let text_l = node.text.to_ascii_lowercase();
    let is_gpu = type_l.contains("gpu")
        || text_l.contains("nvidia")
        || text_l.contains("radeon")
        || text_l.contains("geforce")
        || text_l.contains("intel arc")
        || text_l.contains("intel(r) arc")
        || (text_l.contains("intel") && text_l.contains("graphics"));
    if is_gpu {
        let mut gpu = LhmGpu {
            name: node.text.clone(),
            ..Default::default()
        };
        fill_gpu_metrics(node, &mut gpu);
        if gpu.temp_c.is_some() || gpu.load.is_some() || gpu.mem_total.is_some() {
            out.push(gpu);
        }
    }
    for child in &node.children {
        collect_gpus(child, out);
    }
}

fn fill_gpu_metrics(node: &Node, gpu: &mut LhmGpu) {
    let type_l = node.type_name.to_ascii_lowercase();
    let text_l = node.text.to_ascii_lowercase();
    if type_l.contains("temperature") || text_l.contains("temperature") {
        if gpu.temp_c.is_none() {
            gpu.temp_c = parse_temp_value(&node.value);
        }
    }
    if type_l.contains("load") || text_l.contains("core") && text_l.contains("load") || text_l == "gpu core" {
        if gpu.load.is_none() {
            if let Some(v) = parse_temp_value(&node.value) {
                if (0.0..=100.0).contains(&v) {
                    gpu.load = Some(v);
                }
            }
        }
    }
    if text_l.contains("memory") && (text_l.contains("used") || type_l.contains("data")) {
        if gpu.mem_used.is_none() {
            gpu.mem_used = parse_bytes_value(&node.value);
        }
    }
    if text_l.contains("memory") && (text_l.contains("total") || text_l.contains("available")) {
        // ignore available; prefer total
        if text_l.contains("total") && gpu.mem_total.is_none() {
            gpu.mem_total = parse_bytes_value(&node.value);
        }
    }
    if type_l.contains("fan") || text_l.contains("fan") {
        if gpu.fan_percent.is_none() {
            if let Some(v) = parse_temp_value(&node.value) {
                gpu.fan_percent = Some(v.round().clamp(0.0, 100.0) as u32);
            }
        }
    }
    for child in &node.children {
        fill_gpu_metrics(child, gpu);
    }
}

fn parse_bytes_value(raw: &str) -> Option<u64> {
    let lower = raw.to_ascii_lowercase();
    let num: f64 = {
        let cleaned: String = raw
            .chars()
            .map(|c| if c == ',' { '.' } else { c })
            .filter(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        cleaned.parse().ok()?
    };
    let mult = if lower.contains("gib") || lower.contains("gb") {
        1024.0 * 1024.0 * 1024.0
    } else if lower.contains("mib") || lower.contains("mb") {
        1024.0 * 1024.0
    } else if lower.contains("kib") || lower.contains("kb") {
        1024.0
    } else {
        1.0
    };
    Some((num * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_european_temp_value() {
        assert_eq!(parse_temp_value("39,3 °C"), Some(39.3));
        assert_eq!(parse_temp_value("71.5 C"), Some(71.5));
    }

    #[test]
    fn collects_tctl_from_tree() {
        let json = r#"{
            "Text":"Sensor","Children":[{
                "Text":"AMD Ryzen","Children":[{
                    "Text":"Temperatures","Children":[{
                        "Text":"Core (Tctl/Tdie)","Type":"Temperature","Value":"41,2 °C",
                        "SensorId":"/amdcpu/0/temperature/2","Children":[]
                    }]
                }]
            }]
        }"#;
        let temp = parse_cpu_temp(json, &|temps| {
            temps
                .into_iter()
                .find(|(n, _)| n.to_lowercase().contains("tctl"))
                .map(|(_, t)| t)
        });
        assert_eq!(temp, Some(41.2));
    }
}
