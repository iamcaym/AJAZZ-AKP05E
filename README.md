# akp-deck

Неофициальная бесплатная утилита для LCD-панели **AKP05E** (протокол mirajazz v3)
на Windows: мониторинг ПК на клавишах, громкость приложений на энкодерах, действия
по нажатию и свайпу, настройка в окне.

**Не связана с производителями панели и официальным ПО.** Товарные знаки третьих
сторон упоминаются только для обозначения совместимости.

**Платформа:** Windows 10/11 x64 · **Лицензия:** [MIT](LICENSE)

## Возможности

- 10 LCD-клавиш и 4 зоны тач-стрипа (CPU, RAM, GPU, диск, сеть, часы, заряд)
- 4 энкодера для громкости отдельных приложений Windows
- Сцены и страницы, свайпы, хоткеи, URL, запуск программ
- Трей, автозапуск; HWiNFO для CPU temp (UAC только у HWiNFO при первом старте)
- Конфиг: `%APPDATA%\akp-deck\config.toml`

## Требования

- Windows 10/11 x64
- Панель mirajazz v3 (по умолчанию **AKP05E**, USB `0300:3004`)
- Родное приложение панели (Stream Dock и аналоги) должно быть закрыто — HID эксклюзивный
- Rust 1.75+ для сборки из исходников

## Сборка и запуск

```powershell
cargo build --release
.\target\release\akp-deck.exe
```

Само окно akp-deck открывается без прав администратора. При первом
подъёме HWiNFO Windows может один раз запросить UAC (у HWiNFO в манифесте
`requireAdministrator`).

### Portable-сборка на GitHub

После пуша в репозиторий Actions собирает один `akp-deck.exe` (без установщика).

| Событие | Результат |
|---|---|
| Push / PR в `main` | Артефакт **akp-deck-windows-x64** во вкладке Actions |
| Тег `v0.1.0` и выше | GitHub Release с `akp-deck.exe` + `.sha256` |

```powershell
git tag v0.1.0-beta
git push origin v0.1.0-beta
```

Скачал exe → запустил. Конфиг пишется в `%APPDATA%\akp-deck\`, рядом с exe ничего класть не нужно.

## Настройки

| Параметр | Где |
|---|---|
| Яркость, цвета, интервалы | Шестерёнка → Общие |
| Автозапуск с Windows | Шестерёнка → Общие → Система |
| VID/PID другой панели | `config.toml` → `[device] vid` / `pid` или `AKPDECK_VID` / `AKPDECK_PID` |
| Индекс GPU | `[monitor] gpu_index` |

## Источники метрик

| Метрика | Источник |
|---|---|
| CPU temp | HWiNFO Shared Memory (portable ставится сам) → WMI → уже запущенный LibreHardwareMonitor |
| GPU | NVML (NVIDIA); LibreHardwareMonitor — только если уже запущен |
| Заряд | Aurora/Sinowealth-мыши, DualSense, Bluetooth Battery Service |

При первом запуске на новом ПК HWiNFO portable скачивается в `%LOCALAPPDATA%\akp-deck\hwinfo` и стартует в режиме Sensors only (без окна summary). Иконка в трее — ограничение HWiNFO. Free Shared Memory действует ~12 часов, затем процесс тихо перезапускается.

LibreHardwareMonitor akp-deck **не ставит и не запускает** (только читает HTTP, если LHM уже открыт вручную).

## Структура

```
src/
  main.rs       цикл устройства + UI
  device.rs     HID / mirajazz
  config.rs     config.toml
  monitor.rs    метрики ПК
  battery.rs    заряд периферии
  render.rs     плитки LCD
  ui.rs         окно настроек
  autostart.rs  автозапуск Windows
```

## Ограничения бета

- Только Windows; раскладка 10+4 заточена под AKP05E
- Другие модели — через `vid`/`pid`, без отдельной раскладки
- OBS, GIF, макросы — после беты

## Лицензия

MIT — см. [LICENSE](LICENSE).
