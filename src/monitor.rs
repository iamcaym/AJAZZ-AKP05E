//! Сбор метрик ПК: CPU, память, GPU, диски, сеть.
//!
//! CPU/RAM/диск/сеть — `sysinfo`. GPU: NVML (NVIDIA), иначе LibreHardwareMonitor
//! (только если уже запущен — сами LHM не поднимаем).
//! Температура CPU: HWiNFO Shared Memory (portable ставится автоматически),
//! затем WMI / уже работающий LHM.

#[path = "hwinfo.rs"]
mod hwinfo;
#[path = "lhm_http.rs"]
mod lhm_http;
#[path = "battery.rs"]
mod battery;

pub use battery::{list_devices as list_battery_devices, BatteryDevice, BatteryKind};

use std::time::{Duration, Instant};

use anyhow::Result;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use nvml_wrapper::Nvml;
use sysinfo::{
    Components, CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, RefreshKind,
    System,
};
use windows::core::{w, BSTR, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoTaskMemFree, CoUninitialize,
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{VariantClear, VariantToDouble, VariantToStringAlloc, VARIANT};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, WbemLocator,
    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
};

use crate::config::Metric;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Как часто перечитываем занятость диска.
const DISK_REFRESH: Duration = Duration::from_secs(30);
/// Как часто опрашиваем WMI LibreHardwareMonitor, пока он отвечает.
const LHM_SCAN: Duration = Duration::from_secs(3);
/// Как часто повторяем попытку, если LibreHardwareMonitor ещё не ответил.
const LHM_RETRY: Duration = Duration::from_secs(1);
/// Как часто опрашиваем источник, метрики которого нет в активной сцене:
/// окну нужно знать, доступна ли метрика, и показывать её превью.
const IDLE_REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct GpuInfo {
    pub name: String,
    pub temp_c: Option<f32>,
    pub load: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub fan_percent: Option<u32>,
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct Snapshot {
    pub cpu_load: f32,
    pub cpu_temp: Option<f32>,
    pub cpu_freq_mhz: u64,
    pub cpu_max_core_load: f32,
    pub cpu_thread_count: u32,
    pub ram_percent: f32,
    pub ram_used: u64,
    pub ram_total: u64,
    pub gpu: Option<GpuInfo>,
    pub disk_percent: Option<f32>,
    /// Активное время дисков, %, как в диспетчере задач.
    pub disk_activity: Option<f32>,
    pub disk_used: u64,
    pub disk_total: u64,
    pub net_down_bps: u64,
    pub net_up_bps: u64,
    pub uptime_secs: u64,
    pub clock: String,
    /// Заряд мышей / геймпадов / Bluetooth с Battery Service.
    pub batteries: Vec<BatteryDevice>,
}

pub struct Monitor {
    sys: System,
    networks: Networks,
    disks: Disks,
    components: Option<Components>,
    nvml: Option<Nvml>,
    gpu_index: u32,
    prev_net: Option<(u64, u64, Instant)>,
    lhm_cpu_temp: Option<f32>,
    last_lhm_scan: Option<Instant>,
    last_disk_refresh: Instant,
    gpu_name: String,
    needs: Needs,
    cpu_temp: Option<f32>,
    last_cpu_temp_poll: Option<Instant>,
    gpu: Option<GpuInfo>,
    last_gpu_poll: Option<Instant>,
    disk_counter: Option<DiskActivity>,
    disk_activity: Option<f32>,
    last_disk_activity_poll: Option<Instant>,
    batteries: Vec<BatteryDevice>,
    last_battery_poll: Option<Instant>,
}

/// Какие дорогие источники метрик нужны активной сцене и опрашиваются каждый
/// тик. Остальные обновляются раз в `IDLE_REFRESH`. CPU, память, диск и часы
/// дешёвые или и так редкие.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Needs {
    pub cpu_temp: bool,
    pub gpu: bool,
    pub net: bool,
    pub disk_activity: bool,
    pub battery: bool,
}

impl Needs {
    pub const ALL: Self = Self {
        cpu_temp: true,
        gpu: true,
        net: true,
        disk_activity: true,
        battery: true,
    };

    pub fn from_metrics(metrics: impl IntoIterator<Item = Metric>) -> Self {
        let mut needs = Self {
            cpu_temp: false,
            gpu: false,
            net: false,
            disk_activity: false,
            battery: false,
        };
        for metric in metrics {
            match metric {
                Metric::CpuTemp => needs.cpu_temp = true,
                Metric::GpuTemp | Metric::GpuLoad | Metric::GpuVram | Metric::GpuVramPercent | Metric::GpuFan => needs.gpu = true,
                Metric::NetDown | Metric::NetUp => needs.net = true,
                Metric::DiskActivity => needs.disk_activity = true,
                Metric::DeviceBattery => needs.battery = true,
                _ => {}
            }
        }
        needs
    }
}

/// Счётчик PDH «% Idle Time» по всем физическим дискам. Активность считается
/// как в диспетчере задач: 100 минус время простоя с прошлого замера.
struct DiskActivity {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

// Хэндлы PDH не привязаны к потоку, а монитор опрашивается из одной задачи.
unsafe impl Send for DiskActivity {}

impl DiskActivity {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER(std::ptr::null_mut());
            if PdhAddEnglishCounterW(query, w!("\\PhysicalDisk(_Total)\\% Idle Time"), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            PdhCollectQueryData(query);
            Some(Self { query, counter })
        }
    }

    fn read(&self) -> Option<f32> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let mut value: PDH_FMT_COUNTERVALUE = std::mem::zeroed();
            if PdhGetFormattedCounterValue(self.counter, PDH_FMT_DOUBLE, None, &mut value) != 0 {
                return None;
            }
            let idle = value.Anonymous.doubleValue;
            idle.is_finite().then(|| (100.0 - idle).clamp(0.0, 100.0) as f32)
        }
    }
}

impl Drop for DiskActivity {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

impl Monitor {
    pub fn new(gpu_index: u32) -> Result<Self> {
        let sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        )?;

        let networks = Networks::new_with_refreshed_list()?;
        let disks = Disks::new_with_refreshed_list()?;

        let components = match Components::new_with_refreshed_list() {
            Ok(c) => {
                if (&c).into_iter().next().is_none() {
                    println!(
                        "[monitor] системные датчики температуры пусты; CPU temp через HWiNFO"
                    );
                    None
                } else {
                    for comp in &c {
                        println!(
                            "[monitor] датчик {:?} = {:?} °C",
                            comp.label(),
                            comp.temperature()
                        );
                    }
                    // На Windows sysinfo часто даёт только «Computer» (ACPI zone),
                    // это не CPU. Оставляем список, но cpu_temp() берёт только
                    // явно процессорные подписи; иначе идём в LHM/HWiNFO.
                    if !(&c).into_iter().any(|comp| is_cpu_label(comp.label())) {
                        println!("[monitor] среди системных датчиков нет подписи CPU, ждём HWiNFO");
                    }
                    Some(c)
                }
            }
            Err(e) => {
                println!("[monitor] компоненты температуры недоступны: {e}");
                None
            }
        };

        let nvml = match Nvml::init() {
            Ok(n) => {
                println!("[monitor] NVML инициализирован");
                Some(n)
            }
            Err(e) => {
                println!("[monitor] NVML недоступен ({e}); GPU — только если LHM уже запущен");
                None
            }
        };

        let mut gpu_name = "GPU".to_string();

        if let Some(n) = &nvml {
            match n.device_by_index(gpu_index) {
                Ok(dev) => {
                    if let Ok(name) = dev.name() {
                        gpu_name = name;
                    }
                    println!("[monitor] GPU {gpu_index}: {gpu_name}");
                }
                Err(e) => println!("[monitor] GPU {gpu_index} не открылась: {e}"),
            }
        }

        // Прогрев HWiNFO в фоне: portable + Shared Memory, без окон LHM/SM.
        hwinfo::request_ensure();

        Ok(Self {
            sys,
            networks,
            disks,
            components,
            nvml,
            gpu_index,
            prev_net: None,
            lhm_cpu_temp: None,
            last_lhm_scan: None,
            last_disk_refresh: Instant::now(),
            gpu_name,
            needs: Needs::ALL,
            cpu_temp: None,
            last_cpu_temp_poll: None,
            gpu: None,
            last_gpu_poll: None,
            disk_counter: DiskActivity::open(),
            disk_activity: None,
            last_disk_activity_poll: None,
            batteries: Vec::new(),
            last_battery_poll: None,
        })
    }

    /// Задаёт, какие дорогие источники опрашивать каждый тик. Остальные
    /// обновляются раз в `IDLE_REFRESH`. Возвращает `true`, если набор изменился.
    pub fn set_needs(&mut self, needs: Needs) -> bool {
        let changed = self.needs != needs;
        self.needs = needs;
        changed
    }

    /// Собирает свежий снимок метрик.
    pub fn poll(&mut self) -> Snapshot {
        let now = Instant::now();
        let due = |needed: bool, last: Option<Instant>| {
            needed || last.is_none_or(|last| now.duration_since(last) >= IDLE_REFRESH)
        };

        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        if now.duration_since(self.last_disk_refresh) >= DISK_REFRESH {
            self.disks.refresh_specifics(true, DiskRefreshKind::nothing().with_storage());
            self.last_disk_refresh = now;
        }

        let total = self.sys.total_memory();
        let used = self.sys.used_memory();
        let ram_percent = if total > 0 {
            used as f32 / total as f32 * 100.0
        } else {
            0.0
        };

        let cpu_freq_mhz = self
            .sys
            .cpus()
            .iter()
            .map(|c| c.frequency())
            .max()
            .unwrap_or(0);
        let cpu_max_core_load = self.sys.cpus().iter().map(|cpu| cpu.usage()).fold(0.0f32, f32::max);
        let cpu_thread_count = self.sys.cpus().len() as u32;

        let (net_down_bps, net_up_bps) = if self.needs.net {
            self.networks.refresh(true);
            self.network_speed()
        } else {
            // Иначе после паузы скорость посчиталась бы как среднее за весь простой.
            self.prev_net = None;
            (0, 0)
        };
        let (disk_percent, disk_used, disk_total) = self.disk_usage();

        if due(self.needs.cpu_temp, self.last_cpu_temp_poll) {
            self.last_cpu_temp_poll = Some(now);
            if let Some(c) = &mut self.components {
                c.refresh(false);
            }
            let sensor_cpu_temp = self.cpu_temp();
            let lhm_interval = if self.lhm_cpu_temp.is_some() { LHM_SCAN } else { LHM_RETRY };
            if sensor_cpu_temp.is_none()
                && self.last_lhm_scan.is_none_or(|last| now.duration_since(last) >= lhm_interval)
            {
                self.last_lhm_scan = Some(now);
                self.lhm_cpu_temp = external_cpu_temp();
                if self.lhm_cpu_temp.is_none() {
                    static WARNED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        println!(
                            "[monitor] температура CPU недоступна: ждём HWiNFO Shared Memory"
                        );
                    }
                }
            }
            self.cpu_temp = sensor_cpu_temp.or(self.lhm_cpu_temp);
        }

        if due(self.needs.gpu, self.last_gpu_poll) {
            self.last_gpu_poll = Some(now);
            self.gpu = self.poll_gpu();
        }

        if due(self.needs.disk_activity, self.last_disk_activity_poll) {
            self.last_disk_activity_poll = Some(now);
            self.disk_activity = self.disk_counter.as_ref().and_then(DiskActivity::read);
        }

        if due(self.needs.battery, self.last_battery_poll) {
            self.last_battery_poll = Some(now);
            self.batteries = battery::list_devices();
        }

        Snapshot {
            cpu_load: self.sys.global_cpu_usage(),
            cpu_temp: self.cpu_temp,
            cpu_freq_mhz,
            cpu_max_core_load,
            cpu_thread_count,
            ram_percent,
            ram_used: used,
            ram_total: total,
            gpu: self.gpu.clone(),
            disk_percent,
            disk_activity: self.disk_activity,
            disk_used,
            disk_total,
            net_down_bps,
            net_up_bps,
            uptime_secs: System::uptime().unwrap_or(0),
            clock: chrono::Local::now().format("%H:%M").to_string(),
            batteries: self.batteries.clone(),
        }
    }

    /// Возвращает только явно подписанный CPU-датчик, чтобы не выдавать
    /// температуру системной платы или комнаты за температуру процессора.
    fn cpu_temp(&self) -> Option<f32> {
        let components = self.components.as_ref()?;

        for comp in components {
            let Some(t) = comp.temperature() else { continue };
            if is_cpu_label(comp.label()) {
                return Some(t);
            }
        }
        None
    }

    fn poll_gpu(&self) -> Option<GpuInfo> {
        if let Some(info) = self.poll_gpu_nvml() {
            return Some(info);
        }
        self.poll_gpu_lhm()
    }

    fn poll_gpu_nvml(&self) -> Option<GpuInfo> {
        let nvml = self.nvml.as_ref()?;
        let dev = nvml.device_by_index(self.gpu_index).ok()?;

        let name = self.gpu_name.clone();
        let temp_c = dev
            .temperature(TemperatureSensor::Gpu)
            .ok()
            .map(|t| t as f32);

        let mut load = None;
        let mut mem_used = 0;
        let mut mem_total = 0;

        if let Ok(u) = dev.utilization_rates() {
            load = Some(u.gpu as f32);
            if let Ok(m) = dev.memory_info() {
                mem_used = m.used;
                mem_total = m.total;
            }
        } else if let Ok(m) = dev.memory_info() {
            mem_used = m.used;
            mem_total = m.total;
        }

        let fan_percent = dev.fan_speed(0).ok();

        Some(GpuInfo {
            name,
            temp_c,
            load,
            mem_used,
            mem_total,
            fan_percent,
        })
    }

    fn poll_gpu_lhm(&self) -> Option<GpuInfo> {
        // LHM не поднимаем сами — окно/трей. NVML покрывает NVIDIA; AMD/Intel —
        // если пользователь уже держит LHM, подхватим.
        let snap = lhm_http::gpu_if_running(self.gpu_index as usize)?;
        Some(GpuInfo {
            name: if snap.name.is_empty() {
                self.gpu_name.clone()
            } else {
                snap.name
            },
            temp_c: snap.temp_c,
            load: snap.load,
            mem_used: snap.mem_used.unwrap_or(0),
            mem_total: snap.mem_total.unwrap_or(0),
            fan_percent: snap.fan_percent,
        })
    }

    fn network_speed(&mut self) -> (u64, u64) {
        let mut down_total = 0u64;
        let mut up_total = 0u64;

        for (name, data) in &self.networks {
            let name = name.to_string_lossy().to_lowercase();
            if name.contains("loopback") {
                continue;
            }
            down_total = down_total.saturating_add(data.total_received());
            up_total = up_total.saturating_add(data.total_transmitted());
        }

        let now = Instant::now();
        let speed = match self.prev_net {
            Some((prev_down, prev_up, at)) => {
                let dt = now.duration_since(at).as_secs_f64().max(0.001);
                let down = down_total.saturating_sub(prev_down) as f64 / dt;
                let up = up_total.saturating_sub(prev_up) as f64 / dt;
                (down as u64, up as u64)
            }
            None => (0, 0),
        };

        self.prev_net = Some((down_total, up_total, now));
        speed
    }

    /// Занятость системного диска (там, где стоит Windows).
    fn disk_usage(&self) -> (Option<f32>, u64, u64) {
        let system_drive = std::env::var("SystemDrive")
            .unwrap_or_else(|_| "C:".to_string())
            .to_uppercase();

        let mut chosen: Option<&sysinfo::Disk> = None;
        let mut fallback: Option<&sysinfo::Disk> = None;

        for disk in &self.disks {
            let mount = disk.mount_point().to_string_lossy().to_uppercase();
            if mount.starts_with(&system_drive) {
                chosen = Some(disk);
                break;
            }
            if fallback.is_none() && !disk.is_removable() {
                fallback = Some(disk);
            }
        }

        let Some(disk) = chosen.or(fallback) else {
            return (None, 0, 0);
        };

        let total = disk.total_space();
        let available = disk.available_space();
        let used = total.saturating_sub(available);
        let percent = if total > 0 {
            used as f32 / total as f32 * 100.0
        } else {
            0.0
        };

        (Some(percent), used, total)
    }
}

/// Подпись датчика температуры, относящегося к процессору.
/// Не принимает общее «Computer» из sysinfo (это ACPI thermal zone платы).
fn is_cpu_label(label: &str) -> bool {
    let label = label.to_lowercase();
    if label == "computer" || label.contains("gpu") || label.contains("ambient") || label.contains("motherboard") {
        return false;
    }
    label.contains("cpu")
        || label.contains("package")
        || label.contains("core")
        || label.contains("tctl")
        || label.contains("tdie")
        || label.contains("ccd")
        || label.contains("k10temp")
        || label.contains("zenpower")
}

/// Байты в гибибайты с одним знаком.
pub fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB
}

/// Внешние источники CPU temp: HWiNFO (авто), затем WMI / уже работающий LHM.
fn external_cpu_temp() -> Option<f32> {
    if let Some(temp) = hwinfo_cpu_temp() {
        return Some(temp);
    }
    // Установка/запуск только в фоне — не блокируем тик метрик.
    hwinfo::request_ensure();
    hardware_monitor_cpu_temp().or_else(|| lhm_http::cpu_temp_if_running(select_cpu_temperature))
}

/// COM на потоке: Uninitialize только если именно мы сделали первую инициализацию.
struct ComInit {
    should_uninit: bool,
}

impl ComInit {
    unsafe fn new() -> Option<Self> {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        // S_OK = 0: инициализировали мы. S_FALSE = 1: уже был MTA на потоке.
        if hr.is_err() {
            return None;
        }
        Some(Self {
            should_uninit: hr == HRESULT(0),
        })
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe { CoUninitialize() };
        }
    }
}

/// WMI LibreHardwareMonitor / OpenHardwareMonitor.
fn hardware_monitor_cpu_temp() -> Option<f32> {
    unsafe {
        let _com = ComInit::new()?;
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
        for namespace in ["ROOT\\LibreHardwareMonitor", "ROOT\\OpenHardwareMonitor"] {
            if let Some(temp) = wmi_cpu_temp_from_namespace(&locator, namespace) {
                return Some(temp);
            }
        }
        None
    }
}

unsafe fn wmi_cpu_temp_from_namespace(locator: &IWbemLocator, namespace: &str) -> Option<f32> {
    let namespace = BSTR::from(namespace);
    let empty = BSTR::new();
    let services = locator
        .ConnectServer(&namespace, &empty, &empty, &empty, 0, &empty, None)
        .ok()?;
    CoSetProxyBlanket(
        &services,
        RPC_C_AUTHN_WINNT,
        RPC_C_AUTHZ_NONE,
        PCWSTR::null(),
        RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
    )
    .ok()?;
    // Без Name LIKE: на части систем фильтр WQL отсекает нужные датчики.
    let lang = BSTR::from("WQL");
    let query = BSTR::from("SELECT Name, Value FROM Sensor WHERE SensorType = 'Temperature'");
    let rows = services
        .ExecQuery(
            &lang,
            &query,
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
        .ok()?;

    let mut temps = Vec::new();
    loop {
        let mut row: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0;
        let hr = rows.Next(250, &mut row, &mut returned);
        if returned == 0 || hr.is_err() {
            break;
        }
        let Some(row) = row[0].take() else { continue };
        let mut name = VARIANT::default();
        let mut value = VARIANT::default();
        let got_name = row.Get(w!("Name"), 0, &mut name, None, None).is_ok();
        let got_value = row.Get(w!("Value"), 0, &mut value, None, None).is_ok();
        if got_name && got_value {
            let mut text = VARIANT::default();
            if windows::Win32::System::Variant::VariantChangeType(
                &mut text,
                &name,
                windows::Win32::System::Variant::VAR_CHANGE_FLAGS(0),
                windows::Win32::System::Variant::VT_BSTR,
            )
            .is_ok()
            {
                let allocated = VariantToStringAlloc(&text).ok();
                let label = allocated
                    .as_ref()
                    .and_then(|p| p.to_string().ok())
                    .unwrap_or_default();
                if let Some(p) = allocated {
                    CoTaskMemFree(Some(p.0 as *const _));
                }
                if let Ok(temp) = VariantToDouble(&value) {
                    if temp.is_finite() && (0.0..=130.0).contains(&temp) {
                        temps.push((label, temp as f32));
                    }
                }
            }
            let _ = VariantClear(&mut text);
        }
        let _ = VariantClear(&mut name);
        let _ = VariantClear(&mut value);
    }
    select_cpu_temperature(temps)
}

/// HWiNFO shared memory (Options → Shared Memory Support).
fn hwinfo_cpu_temp() -> Option<f32> {
    unsafe {
        let mapping = OpenFileMappingW(
            FILE_MAP_READ.0,
            false,
            windows::core::w!("Global\\HWiNFO_SENS_SM2"),
        )
        .ok()?;
        struct Mapping(HANDLE);
        impl Drop for Mapping {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let mapping = Mapping(mapping);
        let view = MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, 0);
        if view.Value.is_null() {
            return None;
        }
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                unsafe {
                    let _ = UnmapViewOfFile(self.0);
                }
            }
        }
        let view = View(view);
        let base = view.0.Value as *const u8;

        // HWiNFO_SENSORS_SHARED_MEM2 header, packed.
        if base.is_null() {
            return None;
        }
        let signature = read_u32(base, 0);
        // Байты "HWiS" (48 57 69 53) → LE u32 0x53695748.
        // Раньше стояло 0x53495748 (буква I=0x49 вместо i=0x69) — SHM всегда
        // отбрасывался, температура CPU не читалась.
        if signature != 0x5369_5748 {
            return None;
        }
        // HWiNFO_SENSORS_SHARED_MEM2: poll_time is i64 at offset 12, then
        // reading section fields start at 32.
        let reading_offset = read_u32(base, 32) as usize;
        let reading_size = read_u32(base, 36) as usize;
        let reading_count = read_u32(base, 40) as usize;
        // Element: type + 2 dwords + 3*128 labels + doubles ≈ 428 bytes.
        if reading_size < 400 || reading_count == 0 || reading_count > 4096 {
            return None;
        }

        // HWiNFO_SENSORS_READING_ELEMENT (packed):
        //   tReading:u32, dwSensorIndex:u32, dwReadingID:u32,
        //   szLabelOrig[128], szLabelUser[128], szUnit[16],
        //   Value:f64, ValueMin/Max/Avg…
        // Value at offset 12+128+128+16 = 284. (Не 12+128*3 — unit 16 байт, не 128.)
        // В 8.x элемент вырос до 460 байт (UTF-8 хвост), старые смещения те же.
        const VALUE_OFF: usize = 12 + 128 + 128 + 16;
        let mut temps = Vec::new();
        for index in 0..reading_count {
            let row = base.add(reading_offset + index * reading_size);
            let reading_type = read_u32(row, 0);
            // SENSOR_TYPE_TEMP = 1
            if reading_type != 1 {
                continue;
            }
            if VALUE_OFF + 8 > reading_size {
                break;
            }
            let label = read_c_string(row.add(12), 128);
            let value = read_f64(row, VALUE_OFF);
            // 0 °C — типичный «сенсор ещё не прогрет / пусто», не берём.
            if value.is_finite() && (1.0..=130.0).contains(&value) {
                temps.push((label, value as f32));
            }
        }
        select_cpu_temperature(temps)
    }
}

unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
    u32::from_le_bytes(
        std::ptr::read_unaligned(base.add(offset) as *const [u8; 4]),
    )
}

unsafe fn read_f64(base: *const u8, offset: usize) -> f64 {
    f64::from_le_bytes(
        std::ptr::read_unaligned(base.add(offset) as *const [u8; 8]),
    )
}

unsafe fn read_c_string(ptr: *const u8, max: usize) -> String {
    let mut bytes = Vec::new();
    for i in 0..max {
        let b = *ptr.add(i);
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn select_cpu_temperature(temps: Vec<(String, f32)>) -> Option<f32> {
    let is_preferred = |name: &str| {
        let name = name.to_lowercase();
        !name.contains("gpu")
            && (name.contains("cpu package")
                || name.contains("cpu tctl")
                || name.contains("cpu die")
                || name.contains("tctl")
                || name.contains("tdie")
                || name.contains("cpu (tctl")
                || (name.contains("package") && name.contains("cpu")))
    };
    let is_core = |name: &str| {
        let name = name.to_lowercase();
        !name.contains("gpu") && name.contains("core") && !name.contains("distance")
    };
    let is_other_cpu = |name: &str| {
        let name = name.to_lowercase();
        is_cpu_label(&name) && !is_core(&name)
    };
    if let Some((_, temp)) = temps.iter().find(|(name, _)| is_preferred(name)) {
        return Some(*temp);
    }
    // Среднее по ядрам, если Package не экспортируется.
    let cores: Vec<f32> = temps
        .iter()
        .filter(|(name, _)| is_core(name))
        .map(|(_, temp)| *temp)
        .collect();
    if !cores.is_empty() {
        return Some(cores.iter().sum::<f32>() / cores.len() as f32);
    }
    temps
        .iter()
        .find(|(name, _)| is_other_cpu(name))
        .map(|(_, temp)| *temp)
}

#[cfg(test)]
mod tests {
    use super::select_cpu_temperature;

    #[test]
    fn prefers_cpu_package_sensor_over_other_cores() {
        let sensors = vec![
            ("CPU Core #1".into(), 57.0),
            ("CPU Package".into(), 62.0),
            ("GPU Core".into(), 49.0),
        ];
        assert_eq!(select_cpu_temperature(sensors), Some(62.0));
    }

    #[test]
    fn does_not_report_gpu_temperature_as_cpu() {
        assert_eq!(select_cpu_temperature(vec![("GPU Core".into(), 50.0)]), None);
    }

    #[test]
    fn supports_amd_tctl_temperature_names() {
        assert_eq!(select_cpu_temperature(vec![("CPU (Tctl/Tdie)".into(), 71.5)]), Some(71.5));
    }

    #[test]
    fn hwinfo_signature_is_hwis_lowercase_i() {
        // Байты 48 57 69 53 ("HWiS"), не "HWIS" с I=0x49.
        assert_eq!(u32::from_le_bytes(*b"HWiS"), 0x5369_5748);
        assert_ne!(u32::from_le_bytes(*b"HWiS"), 0x5349_5748);
    }

    #[test]
    fn averages_core_temps_when_package_missing() {
        let sensors = vec![
            ("CPU Core #0".into(), 50.0),
            ("CPU Core #1".into(), 60.0),
            ("GPU Core".into(), 80.0),
        ];
        assert_eq!(select_cpu_temperature(sensors), Some(55.0));
    }

    #[test]
    fn ignores_generic_computer_acpi_label() {
        assert!(!super::is_cpu_label("Computer"));
    }
}
