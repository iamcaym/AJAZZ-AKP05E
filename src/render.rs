//! Отрисовка плиток LCD: 112×112 для клавиш и 176×112 для зон стрипа.
//!
//! Рисуем в обычный RGB, поворот на 180° и JPEG делает `mirajazz`
//! (`convert_image_with_format`) при заливке.

use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use ab_glyph::{FontArc, PxScale};
use image::{Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut, text_size};
use imageproc::rect::Rect as PixelRect;

use crate::config::{Config, Gradient, Metric, Style, TileColors, ZoneCfg};
use crate::monitor::{gib, Snapshot};
use crate::shared::AudioState;

pub const KEY_W: u32 = 112;
pub const KEY_H: u32 = 112;
pub const ZONE_W: u32 = 176;
pub const ZONE_H: u32 = 112;

type Color = Rgb<u8>;

/// Фон плиток и пустых ячеек: чистый чёрный, без серой «подложки».
const BG: Color = Rgb([0, 0, 0]);
/// Только дорожки полос/колец, чуть светлее фона, чтобы полоска читалась.
const BG_SOFT: Color = Rgb([28, 28, 28]);
const TEXT: Color = Rgb([232, 236, 243]);
const MUTED: Color = Rgb([128, 138, 156]);
const BLUE: Color = Rgb([74, 158, 255]);
const GREEN: Color = Rgb([53, 208, 127]);
const YELLOW: Color = Rgb([255, 194, 74]);
const ORANGE: Color = Rgb([255, 146, 48]);
const RED: Color = Rgb([255, 92, 92]);
const GRAY: Color = Rgb([92, 100, 116]);

struct TileText {
    label: String,
    value: String,
    unit: String,
    frac: Option<f32>,
    /// Значение для градиента, только у метрик с `supports_gradient`.
    level: Option<f32>,
}

impl TileText {
    fn missing(label: &str) -> Self {
        Self {
            label: label.to_string(),
            value: "—".to_string(),
            unit: String::new(),
            frac: None,
            level: None,
        }
    }
}

/// Цвета одной плитки после применения общих настроек и своих цветов плитки.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TilePalette {
    pub digit: [u8; 3],
    pub bar: [u8; 3],
    pub gradient: Option<Gradient>,
}

impl TilePalette {
    /// Порядок: свой градиент плитки, свои цвета плитки, общий градиент метрики, общие цвета.
    pub fn resolve(cfg: &Config, colors: &TileColors, metric: Option<Metric>) -> Self {
        let custom = colors.digit.is_some() || colors.bar.is_some();
        let gradient = metric
            .filter(|metric| metric.supports_gradient())
            .and_then(|metric| colors.gradient.or(if custom { None } else { cfg.monitor.gradient(metric) }));
        Self {
            digit: colors.digit.unwrap_or(cfg.device.digit_color),
            bar: colors.bar.unwrap_or(cfg.device.bar_color),
            gradient,
        }
    }

    /// Цвет цифр и цвет полосы для значения метрики.
    fn colors(&self, level: Option<f32>) -> (Color, Color) {
        match (self.gradient, level) {
            (Some(gradient), Some(level)) => {
                let color = gradient_color(gradient, level);
                (color, color)
            }
            _ => (Rgb(self.digit), Rgb(self.bar)),
        }
    }
}

pub struct Renderer {
    bold: FontArc,
    regular: FontArc,
}

/// Шрифты читаются с диска один раз на процесс; `FontArc` дёшево клонируется.
static FONTS: OnceLock<Result<(FontArc, FontArc), String>> = OnceLock::new();

impl Renderer {
    pub fn new() -> Result<Self> {
        let fonts = FONTS.get_or_init(|| {
            let fonts = windows_fonts_dir();
            let bold_paths = [
                fonts.join("segoeuib.ttf"),
                fonts.join("arialbd.ttf"),
            ];
            let regular_paths = [
                fonts.join("segoeui.ttf"),
                fonts.join("arial.ttf"),
            ];
            let bold = load_first_paths(&bold_paths).map_err(|e| format!("{e:#}"))?;
            let regular =
                load_first_paths(&regular_paths).unwrap_or_else(|_| bold.clone());

            Ok((bold, regular))
        });
        let (bold, regular) = fonts.clone().map_err(|e| anyhow!(e))?;

        Ok(Self { bold, regular })
    }

    /// Плитка клавиши под выбранную метрику и стиль.
    pub fn key_tile(
        &self,
        metric: Metric,
        style: Style,
        snap: &Snapshot,
        palette: TilePalette,
        battery_device: Option<&str>,
    ) -> RgbImage {
        if metric == Metric::DeviceBattery {
            return self.battery_key_tile(snap, battery_device, style, palette);
        }
        let text = metric_text(metric, snap, None);
        let (digit_color, bar_color) = palette.colors(text.level);
        match style {
            Style::Value => self.value_tile(&text, digit_color),
            Style::Bar => self.bar_tile(&text, digit_color, bar_color),
            Style::Ring => self.ring_tile(&text, digit_color, bar_color),
        }
    }

    /// Заряд: две колонки — иконка+процент слева, вертикальный бар справа.
    fn battery_key_tile(
        &self,
        snap: &Snapshot,
        battery_device: Option<&str>,
        _style: Style,
        palette: TilePalette,
    ) -> RgbImage {
        let Some(id) = battery_device.map(str::trim).filter(|id| !id.is_empty()) else {
            return self.blank_tile(KEY_W, KEY_H);
        };
        let Some(dev) = snap.batteries.iter().find(|d| d.id == id) else {
            return self.blank_tile(KEY_W, KEY_H);
        };
        let text = metric_text(Metric::DeviceBattery, snap, Some(id));
        let (digit_color, bar_color) = palette.colors(text.level);
        let mut img = RgbImage::from_pixel(KEY_W, KEY_H, BG);

        // Левая колонка ~54px, правая — жирный вертикальный бар.
        let left_cx = 28;
        draw_device_icon_large(&mut img, dev.kind, left_cx, 28, digit_color);

        let value = text.value.clone();
        let value_scale = self.fit(&self.bold, &value, 48.0, 28.0, 14.0);
        let (vw, vh) = text_size(PxScale::from(value_scale), &self.bold, &value);
        let (uw, uh) = text_size(PxScale::from(14.0), &self.regular, "%");
        let group_w = vw as i32 + uw as i32 + 2;
        let vx = left_cx - group_w / 2;
        let vy = 78 - vh as i32 / 2;
        draw_text_mut(
            &mut img,
            digit_color,
            vx,
            vy,
            PxScale::from(value_scale),
            &self.bold,
            &value,
        );
        draw_text_mut(
            &mut img,
            if dev.charging { YELLOW } else { MUTED },
            vx + vw as i32 + 2,
            vy + vh as i32 - uh as i32 - 1,
            PxScale::from(14.0),
            &self.regular,
            "%",
        );

        vbar(
            &mut img,
            78,
            10,
            18,
            KEY_H - 20,
            text.frac.unwrap_or(0.0),
            bar_color,
        );
        img
    }

    /// Плитка зоны стрипа: название и значение с полосой.
    pub fn volume_tile(&self, title: &str, volume: i32, palette: TilePalette) -> RgbImage {
        let digit_color = palette.digit;
        let volume = volume.clamp(0, 100);
        let color = if volume == 0 {
            GRAY
        } else if volume >= 90 {
            YELLOW
        } else {
            Rgb(palette.bar)
        };

        let mut img = RgbImage::from_pixel(ZONE_W, ZONE_H, BG);

        let title_scale = self.fit(&self.regular, &title.to_uppercase(), ZONE_W as f32 - 20.0, 27.0, 15.0);
        let title = self.truncate(&title.to_uppercase(), ZONE_W as f32 - 20.0, title_scale);
        self.text_center_top(&mut img, &title, ZONE_W as i32 / 2, 2, title_scale, MUTED, false);

        let value = format!("{volume}");
        let scale = self.fit(&self.bold, &value, 96.0, 50.0, 20.0);
        let (w, h) = text_size(PxScale::from(scale), &self.bold, &value);

        let (unit_w, unit_h) = text_size(PxScale::from(12.0), &self.regular, "%");
        let right = ZONE_W as i32 - 14;
        let x = right - unit_w as i32 - 4 - w as i32;
        let y = 30 + (36 - h.min(36) as i32) / 2;

        draw_text_mut(
            &mut img,
            Rgb(digit_color),
            x,
            y,
            PxScale::from(scale),
            &self.bold,
            &value,
        );
        draw_text_mut(
            &mut img,
            Rgb(digit_color),
            right - unit_w as i32,
            y + h as i32 - unit_h as i32 - 2,
            PxScale::from(12.0),
            &self.regular,
            "%",
        );

        bar(
            &mut img,
            14,
            88,
            ZONE_W - 28,
            10,
            volume as f32 / 100.0,
            color,
        );

        img
    }

    /// Плитка зоны с привязанным приложением: иконка по центру, снизу —
    /// громкость этой сессии (или `MUTE`/`off`).
    pub fn app_tile(
        &self,
        title: &str,
        icon: Option<&RgbaImage>,
        volume: i32,
        muted: bool,
        active: bool,
        palette: TilePalette,
    ) -> RgbImage {
        let digit_color = palette.digit;
        let volume = volume.clamp(0, 100);
        let color = if muted {
            YELLOW
        } else if !active || volume == 0 {
            GRAY
        } else if volume >= 90 {
            YELLOW
        } else {
            Rgb(palette.bar)
        };

        let mut img = RgbImage::from_pixel(ZONE_W, ZONE_H, BG);

        let title = title.trim();
        let has_title = !title.is_empty();
        if has_title {
            let title_scale = self.fit(&self.regular, &title.to_uppercase(), ZONE_W as f32 - 12.0, 20.0, 16.0);
            let label = self.truncate(&title.to_uppercase(), ZONE_W as f32 - 12.0, title_scale);
            self.text_center_top(&mut img, &label, ZONE_W as i32 / 2, 2, title_scale, MUTED, false);
        }

        // Над индикатором — две равные колонки: крупная иконка и значение.
        // Центрируем каждое содержимое независимо, чтобы разные иконки не
        // сдвигали проценты и не выглядели приплюснутыми.
        let row_top = if has_title { 22i32 } else { 0i32 };
        let row_bottom = 102i32;
        let row_center = (row_top + row_bottom) / 2;
        let column_width = ZONE_W as i32 / 2;
        let icon_size = if has_title { 72u32 } else { 84u32 };
        let icon_top = row_center - icon_size as i32 / 2;
        let icon_left = (column_width - icon_size as i32) / 2;
        match icon {
            Some(icon) => {
                let square = square_icon(icon, icon_size);
                blit_rgba(&mut img, &square, icon_left, icon_top);
            }
            None => {
                draw_filled_rect_mut(
                    &mut img,
                    PixelRect::at(icon_left, icon_top).of_size(icon_size, icon_size),
                    BG_SOFT,
                );
                self.text_center(
                    &mut img,
                    "?",
                    icon_left + icon_size as i32 / 2,
                    icon_top + icon_size as i32 / 2,
                    if has_title { 40.0 } else { 48.0 },
                    GRAY,
                    true,
                );
            }
        }

        let value = if muted {
            "MUTE".to_string()
        } else if !active {
            "off".to_string()
        } else {
            format!("{volume}")
        };
        let display_value = if active && !muted { format!("{value}%") } else { value };
        let scale = self.fit(&self.bold, &display_value, column_width as f32 - 4.0, if has_title { 38.0 } else { 44.0 }, 20.0);
        let (w, h) = text_size(PxScale::from(scale), &self.bold, &display_value);
        let vx = column_width + (column_width - w as i32) / 2;
        let vy = row_center - h as i32 / 2;
        draw_text_mut(&mut img, Rgb(digit_color), vx, vy, PxScale::from(scale), &self.bold, &display_value);

        let frac = if muted || !active {
            0.0
        } else {
            volume as f32 / 100.0
        };
        bar(&mut img, 14, 104, ZONE_W - 28, 6, frac, color);

        img
    }

    /// Метрика на сенсоре: число / полоса / кольцо, как на клавишах.
    fn zone_metric_tile(
        &self,
        zone: &ZoneCfg,
        metric: Metric,
        snapshot: &Snapshot,
        palette: TilePalette,
    ) -> RgbImage {
        if metric == Metric::DeviceBattery {
            return self.battery_zone_tile(zone, snapshot, palette);
        }
        let t = metric_text(metric, snapshot, zone.battery_device.as_deref());
        let (digit_color, bar_color) = palette.colors(t.level);
        let title = zone.title.trim();
        let title = if title.is_empty() {
            metric.label()
        } else {
            title
        };

        match zone.style {
            Style::Value => self.zone_value_metric(&t, title, digit_color),
            Style::Bar => self.zone_bar_metric(&t, title, digit_color, bar_color),
            Style::Ring => self.zone_ring_metric(&t, title, digit_color, bar_color),
        }
    }

    /// Заряд на сенсоре: та же схема 2 колонок, шире под 176×112.
    fn battery_zone_tile(
        &self,
        zone: &ZoneCfg,
        snapshot: &Snapshot,
        palette: TilePalette,
    ) -> RgbImage {
        let Some(id) = zone
            .battery_device
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return self.blank_tile(ZONE_W, ZONE_H);
        };
        let Some(dev) = snapshot.batteries.iter().find(|d| d.id == id) else {
            return self.blank_tile(ZONE_W, ZONE_H);
        };
        let text = metric_text(Metric::DeviceBattery, snapshot, Some(id));
        if text.label.is_empty() {
            return self.blank_tile(ZONE_W, ZONE_H);
        }
        let (digit_color, bar_color) = palette.colors(text.level);
        let mut img = RgbImage::from_pixel(ZONE_W, ZONE_H, BG);

        let left_cx = 52;
        draw_device_icon_large(&mut img, dev.kind, left_cx, 30, digit_color);

        let value = text.value.clone();
        let value_scale = self.fit(&self.bold, &value, 70.0, 32.0, 16.0);
        let (vw, vh) = text_size(PxScale::from(value_scale), &self.bold, &value);
        let (uw, uh) = text_size(PxScale::from(16.0), &self.regular, "%");
        let group_w = vw as i32 + uw as i32 + 3;
        let vx = left_cx - group_w / 2;
        let vy = 78 - vh as i32 / 2;
        draw_text_mut(
            &mut img,
            digit_color,
            vx,
            vy,
            PxScale::from(value_scale),
            &self.bold,
            &value,
        );
        draw_text_mut(
            &mut img,
            MUTED,
            vx + vw as i32 + 3,
            vy + vh as i32 - uh as i32 - 1,
            PxScale::from(16.0),
            &self.regular,
            "%",
        );

        vbar(
            &mut img,
            128,
            8,
            28,
            ZONE_H - 16,
            text.frac.unwrap_or(0.0),
            bar_color,
        );
        img
    }

    fn zone_value_metric(&self, t: &TileText, title: &str, digit_color: Color) -> RgbImage {
        let mut img = RgbImage::from_pixel(ZONE_W, ZONE_H, BG);
        let title_scale = self.fit(&self.regular, &title.to_uppercase(), ZONE_W as f32 - 18.0, 23.0, 13.0);
        let title = self.truncate(&title.to_uppercase(), ZONE_W as f32 - 18.0, title_scale);
        self.text_center_top(&mut img, &title, ZONE_W as i32 / 2, 2, title_scale, MUTED, false);

        let value_scale = self.fit(&self.bold, &t.value, ZONE_W as f32 - 26.0, 42.0, 18.0);
        let (value_w, value_h) = text_size(PxScale::from(value_scale), &self.bold, &t.value);
        let unit_scale = 24.0;
        let (unit_w, unit_h) = if t.unit.is_empty() {
            (0, 0)
        } else {
            text_size(PxScale::from(unit_scale), &self.regular, &t.unit)
        };
        let combined_w = value_w as i32 + if t.unit.is_empty() { 0 } else { unit_w as i32 + 4 };
        let left = (ZONE_W as i32 - combined_w) / 2;
        let top = ZONE_H as i32 / 2 - (value_h as i32 + unit_h as i32) / 2;
        draw_text_mut(
            &mut img,
            digit_color,
            left,
            top,
            PxScale::from(value_scale),
            &self.bold,
            &t.value,
        );
        if !t.unit.is_empty() {
            draw_text_mut(
                &mut img,
                MUTED,
                left + value_w as i32 + 4,
                top + value_h as i32 - unit_h as i32 - 2,
                PxScale::from(unit_scale),
                &self.regular,
                &t.unit,
            );
        }
        img
    }

    fn zone_bar_metric(
        &self,
        t: &TileText,
        title: &str,
        digit_color: Color,
        bar_color: Color,
    ) -> RgbImage {
        let mut img = self.zone_value_metric(t, title, digit_color);
        bar(
            &mut img,
            14,
            96,
            ZONE_W - 28,
            8,
            t.frac.unwrap_or(0.0),
            bar_color,
        );
        img
    }

    fn zone_ring_metric(
        &self,
        t: &TileText,
        title: &str,
        digit_color: Color,
        bar_color: Color,
    ) -> RgbImage {
        let mut img = RgbImage::from_pixel(ZONE_W, ZONE_H, BG);
        let title_scale = self.fit(&self.regular, &title.to_uppercase(), ZONE_W as f32 - 18.0, 20.0, 12.0);
        let title = self.truncate(&title.to_uppercase(), ZONE_W as f32 - 18.0, title_scale);
        self.text_center_top(&mut img, &title, ZONE_W as i32 / 2, 1, title_scale, MUTED, false);

        let cx = ZONE_W as f32 / 2.0;
        let cy = 64.0;
        let radius = 28.0;
        let thickness = 6.0;
        draw_arc(&mut img, cx, cy, radius, thickness, 225.0, 270.0, BG_SOFT);
        if let Some(f) = t.frac {
            let sweep = 270.0 * f.clamp(0.0, 1.0);
            if sweep > 0.5 {
                draw_arc(&mut img, cx, cy, radius, thickness, 225.0, sweep, bar_color);
            }
        }
        let scale = self.fit(&self.bold, &t.value, 72.0, 28.0, 14.0);
        self.text_center(&mut img, &t.value, cx as i32, cy as i32, scale, digit_color, true);
        img
    }

    /// Плитка зоны по её конфигурации: обычная громкость или приложение.
    /// Значения берём переданные (они всегда свежие на момент заливки),
    /// а микшер нужен только чтобы понять, запущено ли приложение.
    pub fn zone_tile(
        &self,
        zone: &ZoneCfg,
        snapshot: &Snapshot,
        volume: i32,
        muted: bool,
        audio: &AudioState,
        palette: TilePalette,
    ) -> RgbImage {
        if let Some(metric) = zone.metric {
            return self.zone_metric_tile(zone, metric, snapshot, palette);
        }
        match zone.app.as_deref() {
            Some(app) => self.app_tile(
                &zone.title,
                audio.icon(app),
                volume,
                muted,
                audio.session(app).is_some(),
                palette,
            ),
            // Пустой сенсор без приложения и без метрики на устройстве не рисуем.
            None => self.blank_tile(ZONE_W, ZONE_H),
        }
    }

    /// Служебная плитка: подпись сверху, крупное значение по центру.
    /// Пустая плитка фонового цвета для незанятой ячейки.
    pub fn blank_tile(&self, width: u32, height: u32) -> RgbImage {
        RgbImage::from_pixel(width, height, BG)
    }

    pub fn message_tile(&self, label: &str, value: &str, color: Color) -> RgbImage {
        let mut img = RgbImage::from_pixel(KEY_W, KEY_H, BG);
        let label = self.truncate(&label.to_uppercase(), 94.0, 27.0);
        self.text_center_top(&mut img, &label, 56, 3, 27.0, MUTED, false);
        self.text_center(&mut img, value, 56, 62, 30.0, color, true);
        img
    }

    fn value_tile(&self, t: &TileText, digit_color: Rgb<u8>) -> RgbImage {
        let mut img = RgbImage::from_pixel(KEY_W, KEY_H, BG);
        let label = self.truncate(&t.label.to_uppercase(), 94.0, 28.0);
        self.text_center_top(&mut img, &label, 56, 2, 28.0, MUTED, false);
        self.value_block(&mut img, t, 56, 96.0, 48.0, digit_color);

        img
    }

    fn bar_tile(&self, t: &TileText, digit_color: Rgb<u8>, bar_color: Rgb<u8>) -> RgbImage {
        let mut img = RgbImage::from_pixel(KEY_W, KEY_H, BG);
        let label = self.truncate(&t.label.to_uppercase(), 94.0, 28.0);
        self.text_center_top(&mut img, &label, 56, 2, 28.0, MUTED, false);
        self.value_block(&mut img, t, 52, 96.0, 48.0, digit_color);
        bar(&mut img, 10, 92, KEY_W - 20, 8, t.frac.unwrap_or(0.0), bar_color);
        img
    }

    fn ring_tile(&self, t: &TileText, digit_color: Rgb<u8>, bar_color: Rgb<u8>) -> RgbImage {
        let mut img = RgbImage::from_pixel(KEY_W, KEY_H, BG);

        let cx = 56.0;
        let cy = 66.0;
        let radius = 35.0;
        let thickness = 7.0;

        let label = if t.unit.is_empty() {
            t.label.to_uppercase()
        } else {
            format!("{} {}", t.label.to_uppercase(), t.unit)
        };
        let label = self.truncate(&label, 94.0, 28.0);
        self.text_center_top(&mut img, &label, 56, 2, 28.0, MUTED, false);

        draw_arc(&mut img, cx, cy, radius, thickness, 225.0, 270.0, BG_SOFT);
        if let Some(f) = t.frac {
            let sweep = 270.0 * f.clamp(0.0, 1.0);
            if sweep > 0.5 {
                draw_arc(&mut img, cx, cy, radius, thickness, 225.0, sweep, bar_color);
            }
        }

        let scale = self.fit(&self.bold, &t.value, 62.0, 38.0, 14.0);
        self.text_center(&mut img, &t.value, 56, 66, scale, digit_color, true);

        img
    }

    /// Рисует значение с единицей измерения, центрируя по горизонтали
    /// относительно центра плитки.
    fn value_block(
        &self,
        img: &mut RgbImage,
        t: &TileText,
        center_y: i32,
        max_w: f32,
        start_scale: f32,
        digit_color: Rgb<u8>,
    ) {
        let scale = self.fit(&self.bold, &t.value, max_w, start_scale, 16.0);
        let (w, h) = text_size(PxScale::from(scale), &self.bold, &t.value);
        let unit_scale = 24.0;
        let (uw, uh) = if t.unit.is_empty() { (0, 0) } else { text_size(PxScale::from(unit_scale), &self.regular, &t.unit) };
        let group_width = w as i32 + if t.unit.is_empty() { 0 } else { uw as i32 + 3 };
        let group_left = (KEY_W as i32 - group_width) / 2;
        let vx = group_left;
        let vy = center_y - h as i32 / 2;

        draw_text_mut(
            img,
            digit_color,
            vx,
            vy,
            PxScale::from(scale),
            &self.bold,
            &t.value,
        );

        if t.unit.is_empty() {
            return;
        }

        let ux = vx + w as i32 + 3;
        let uy = vy + h as i32 - uh as i32 - 2;

        if ux + uw as i32 <= KEY_W as i32 - 6 {
            draw_text_mut(
                img,
                MUTED,
                ux,
                uy,
                PxScale::from(unit_scale),
                &self.regular,
                &t.unit,
            );
        } else {
            let bx = (KEY_W as i32 - uw as i32) / 2;
            draw_text_mut(
                img,
                MUTED,
                bx,
                vy + h as i32 + 1,
                PxScale::from(unit_scale),
                &self.regular,
                &t.unit,
            );
        }
    }

    fn text(
        &self,
        img: &mut RgbImage,
        text: &str,
        x: i32,
        y: i32,
        scale: f32,
        color: Color,
        bold: bool,
    ) {
        let font = if bold { &self.bold } else { &self.regular };
        draw_text_mut(img, color, x, y, PxScale::from(scale), font, text);
    }

    fn text_center(
        &self,
        img: &mut RgbImage,
        text: &str,
        cx: i32,
        cy: i32,
        scale: f32,
        color: Color,
        bold: bool,
    ) {
        let font = if bold { &self.bold } else { &self.regular };
        let (w, h) = text_size(PxScale::from(scale), font, text);
        draw_text_mut(
            img,
            color,
            cx - w as i32 / 2,
            cy - h as i32 / 2,
            PxScale::from(scale),
            font,
            text,
        );
    }

    fn text_center_top(
        &self,
        img: &mut RgbImage,
        text: &str,
        cx: i32,
        y: i32,
        scale: f32,
        color: Color,
        bold: bool,
    ) {
        let font = if bold { &self.bold } else { &self.regular };
        let (w, _) = text_size(PxScale::from(scale), font, text);
        draw_text_mut(
            img,
            color,
            cx - w as i32 / 2,
            y,
            PxScale::from(scale),
            font,
            text,
        );
    }

    fn fit(&self, font: &FontArc, text: &str, max_w: f32, start: f32, min: f32) -> f32 {
        let mut scale = start;
        while scale > min {
            let (w, _) = text_size(PxScale::from(scale), font, text);
            if w as f32 <= max_w {
                break;
            }
            scale -= 1.0;
        }
        scale
    }

    /// Обрезает подпись по ширине, добавляя многоточие.
    fn truncate(&self, text: &str, max_w: f32, scale: f32) -> String {
        if text_size(PxScale::from(scale), &self.regular, text).0 as f32 <= max_w {
            return text.to_string();
        }

        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let candidate = format!("{}…", chars.iter().collect::<String>());
            if text_size(PxScale::from(scale), &self.regular, &candidate).0 as f32 <= max_w {
                return candidate;
            }
        }
        "…".to_string()
    }
}

fn windows_fonts_dir() -> std::path::PathBuf {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    std::path::PathBuf::from(windir).join("Fonts")
}

fn load_first_paths(paths: &[std::path::PathBuf]) -> Result<FontArc> {
    for path in paths {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = FontArc::try_from_vec(bytes) {
                return Ok(font);
            }
        }
    }
    Err(anyhow!("не найден ни один шрифт из списка {paths:?}"))
}

fn bar(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, frac: f32, color: Color) {
    draw_filled_rect_mut(img, PixelRect::at(x as i32, y as i32).of_size(w, h), BG_SOFT);
    let filled = (w as f32 * frac.clamp(0.0, 1.0)).round() as u32;
    if filled >= 2 {
        draw_filled_rect_mut(
            img,
            PixelRect::at(x as i32, y as i32).of_size(filled, h),
            color,
        );
    }
}

/// Вертикальная полоса заряда: заполняется снизу вверх.
fn vbar(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, frac: f32, color: Color) {
    draw_filled_rect_mut(img, PixelRect::at(x as i32, y as i32).of_size(w, h), BG_SOFT);
    let filled = (h as f32 * frac.clamp(0.0, 1.0)).round() as u32;
    if filled >= 2 {
        let top = y + h.saturating_sub(filled);
        draw_filled_rect_mut(
            img,
            PixelRect::at(x as i32, top as i32).of_size(w, filled),
            color,
        );
    }
}

/// Вписывает иконку в квадрат, сохраняя пропорции и прозрачные поля.
fn square_icon(icon: &RgbaImage, size: u32) -> RgbaImage {
    let (w, h) = icon.dimensions();
    if w == 0 || h == 0 {
        return RgbaImage::new(size, size);
    }

    let scale = (size as f32 / w as f32).min(size as f32 / h as f32);
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(icon, nw, nh, image::imageops::FilterType::Triangle);

    let mut out = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
    image::imageops::overlay(
        &mut out,
        &resized,
        ((size - nw) / 2) as i64,
        ((size - nh) / 2) as i64,
    );
    out
}

/// Накладывает RGBA-картинку на RGB-плитку с учётом альфы.
fn blit_rgba(dst: &mut RgbImage, src: &RgbaImage, ox: i32, oy: i32) {
    let width = dst.width() as i32;
    let height = dst.height() as i32;

    for (x, y, pixel) in src.enumerate_pixels() {
        let dx = ox + x as i32;
        let dy = oy + y as i32;
        if dx < 0 || dy < 0 || dx >= width || dy >= height {
            continue;
        }

        let alpha = pixel[3] as u32;
        if alpha == 0 {
            continue;
        }

        let target = dst.get_pixel_mut(dx as u32, dy as u32);
        for channel in 0..3 {
            let src_c = pixel[channel] as u32;
            let dst_c = target[channel] as u32;
            target[channel] = ((src_c * alpha + dst_c * (255 - alpha)) / 255) as u8;
        }
    }
}

/// Дуга толщиной `thick` с центром `(cx, cy)`; углы в градусах,
/// 0° — вверх, отсчёт по часовой стрелке.
fn draw_arc(
    img: &mut RgbImage,
    cx: f32,
    cy: f32,
    radius: f32,
    thick: f32,
    start: f32,
    sweep: f32,
    color: Color,
) {
    if sweep <= 0.0 {
        return;
    }

    let outer = radius + thick * 0.5;
    let inner = radius - thick * 0.5;
    let end = start + sweep;

    let x0 = (cx - outer).floor().max(0.0) as i32;
    let x1 = (cx + outer)
        .ceil()
        .min(img.width().saturating_sub(1) as f32) as i32;
    let y0 = (cy - outer).floor().max(0.0) as i32;
    let y1 = (cy + outer)
        .ceil()
        .min(img.height().saturating_sub(1) as f32) as i32;

    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();

            let cover = (outer - dist).min(dist - inner).min(1.0);
            if cover <= 0.0 {
                continue;
            }

            let mut angle = dy.atan2(dx).to_degrees() + 90.0;
            if angle < 0.0 {
                angle += 360.0;
            }
            let angle = if angle < start { angle + 360.0 } else { angle };

            if angle < start || angle > end {
                continue;
            }

            let pixel = img.get_pixel_mut(x as u32, y as u32);
            blend(pixel, color, cover);
        }
    }
}

fn blend(dst: &mut Color, src: Color, alpha: f32) {
    let a = alpha.clamp(0.0, 1.0);
    for i in 0..3 {
        let d = dst.0[i] as f32;
        let s = src.0[i] as f32;
        dst.0[i] = (d + (s - d) * a).round() as u8;
    }
}

/// Плавный переход зелёный - оранжевый - красный по границам градиента.
fn gradient_color(gradient: Gradient, value: f32) -> Color {
    if value <= gradient.green {
        GREEN
    } else if value < gradient.orange {
        mix(GREEN, ORANGE, (value - gradient.green) / (gradient.orange - gradient.green))
    } else if value < gradient.red {
        mix(ORANGE, RED, (value - gradient.orange) / (gradient.red - gradient.orange))
    } else {
        RED
    }
}

fn mix(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Rgb(std::array::from_fn(|i| (from.0[i] as f32 + (to.0[i] as f32 - from.0[i] as f32) * t).round() as u8))
}

fn speed_text(bps: u64) -> (String, String) {
    let b = bps as f64;
    if b >= 1_000_000.0 {
        (format!("{:.1}", b / 1_000_000.0), "MB/s".to_string())
    } else if b >= 1_000.0 {
        (format!("{:.0}", b / 1_000.0), "KB/s".to_string())
    } else {
        (format!("{b:.0}"), "B/s".to_string())
    }
}

fn short_device_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "DEVICE".into();
    }
    let upper = name.to_uppercase();
    if upper.contains("MAYA") {
        return "MAYA X".into();
    }
    if upper.contains("DUALSENSE EDGE") {
        return "DS EDGE".into();
    }
    if upper.contains("DUALSENSE") || upper.contains("WIRELESS CONTROLLER") {
        return "DS5".into();
    }
    if upper.contains("LAMZU") {
        return "LAMZU".into();
    }
    let truncated = name.chars().take(10).collect::<String>();
    truncated.to_uppercase()
}

/// Крупная иконка устройства; `(cx, cy)` — центр.
fn draw_device_icon_large(
    img: &mut RgbImage,
    kind: crate::monitor::BatteryKind,
    cx: i32,
    cy: i32,
    color: Color,
) {
    match kind {
        crate::monitor::BatteryKind::Mouse => {
            let x = cx - 12;
            let y = cy - 18;
            draw_filled_rect_mut(img, PixelRect::at(x + 4, y).of_size(16, 28), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 9, y + 3).of_size(6, 8), BG);
            draw_filled_rect_mut(img, PixelRect::at(x + 11, y + 28).of_size(4, 5), color);
        }
        crate::monitor::BatteryKind::Controller => {
            let x = cx - 22;
            let y = cy - 12;
            draw_filled_rect_mut(img, PixelRect::at(x, y + 8).of_size(44, 14), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 4, y + 2).of_size(10, 10), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 30, y + 2).of_size(10, 10), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 2, y + 20).of_size(6, 8), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 36, y + 20).of_size(6, 8), color);
        }
        crate::monitor::BatteryKind::Bluetooth => {
            // Наушники
            let x = cx - 18;
            let y = cy - 14;
            draw_filled_rect_mut(img, PixelRect::at(x + 6, y).of_size(24, 4), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 4, y + 2).of_size(4, 14), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 28, y + 2).of_size(4, 14), color);
            draw_filled_rect_mut(img, PixelRect::at(x, y + 14).of_size(10, 14), color);
            draw_filled_rect_mut(img, PixelRect::at(x + 26, y + 14).of_size(10, 14), color);
        }
    }
}

fn uptime_text(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn metric_text(metric: Metric, s: &Snapshot, battery_device: Option<&str>) -> TileText {
    match metric {
        Metric::DeviceBattery => {
            let Some(id) = battery_device.map(str::trim).filter(|id| !id.is_empty()) else {
                return TileText::missing("");
            };
            match s.batteries.iter().find(|d| d.id == id) {
                Some(dev) => {
                    let short = short_device_name(&dev.name);
                    match dev.percent {
                        Some(pct) => TileText {
                            label: short,
                            value: format!("{pct}"),
                            unit: if dev.charging { "⚡%".into() } else { "%".into() },
                            frac: Some(pct as f32 / 100.0),
                            // Инвертируем: низкий заряд → высокий level → красный.
                            level: Some(100.0 - pct as f32),
                        },
                        None => TileText {
                            label: short,
                            value: "—".into(),
                            unit: "%".into(),
                            frac: None,
                            level: None,
                        },
                    }
                }
                None => TileText::missing(""),
            }
        }
        Metric::CpuTemp => {
            let t = s.cpu_temp;
            TileText {
                label: "CPU".into(),
                value: t.map(|v| format!("{v:.0}")).unwrap_or_else(|| "—".into()),
                unit: "°C".into(),
                frac: t.map(|v| (v / 100.0).clamp(0.0, 1.0)),
                level: t,
            }
        }
        Metric::CpuLoad => TileText {
            label: "CPU".into(),
            value: format!("{:.0}", s.cpu_load),
            unit: "%".into(),
            frac: Some((s.cpu_load / 100.0).clamp(0.0, 1.0)),
            level: Some(s.cpu_load),
        },
        Metric::CpuFreq => TileText {
            label: "CPU".into(),
            value: format!("{:.1}", s.cpu_freq_mhz as f64 / 1000.0),
            unit: "GHz".into(),
            frac: None,
            level: None,
        },
        Metric::CpuMaxCore => TileText {
            label: "CPU MAX".into(),
            value: format!("{:.0}", s.cpu_max_core_load),
            unit: "%".into(),
            frac: Some((s.cpu_max_core_load / 100.0).clamp(0.0, 1.0)),
            level: Some(s.cpu_max_core_load),
        },
        Metric::CpuThreads => TileText {
            label: "CPU THREADS".into(),
            value: s.cpu_thread_count.to_string(),
            unit: "шт.".into(),
            frac: None,
            level: None,
        },
        Metric::RamPercent => TileText {
            label: "RAM".into(),
            value: format!("{:.0}", s.ram_percent),
            unit: "%".into(),
            frac: Some((s.ram_percent / 100.0).clamp(0.0, 1.0)),
            level: Some(s.ram_percent),
        },
        Metric::RamUsed => TileText {
            label: "RAM".into(),
            value: format!("{:.1}", gib(s.ram_used)),
            unit: "GB".into(),
            frac: Some((s.ram_percent / 100.0).clamp(0.0, 1.0)),
            level: None,
        },
        Metric::RamTotal => TileText {
            label: "RAM TOTAL".into(),
            value: format!("{:.1}", gib(s.ram_total)),
            unit: "GB".into(),
            frac: None,
            level: None,
        },
        Metric::GpuTemp => match &s.gpu {
            Some(g) => TileText {
                label: "GPU".into(),
                value: g
                    .temp_c
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_else(|| "—".into()),
                unit: "°C".into(),
                frac: g.temp_c.map(|v| (v / 100.0).clamp(0.0, 1.0)),
                level: g.temp_c,
            },
            None => TileText::missing("GPU"),
        },
        Metric::GpuLoad => match &s.gpu {
            Some(g) => TileText {
                label: "GPU".into(),
                value: g
                    .load
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_else(|| "—".into()),
                unit: "%".into(),
                frac: g.load.map(|v| (v / 100.0).clamp(0.0, 1.0)),
                level: g.load,
            },
            None => TileText::missing("GPU"),
        },
        Metric::GpuVram => match &s.gpu {
            Some(g) if g.mem_total > 0 => TileText {
                label: "VRAM".into(),
                value: format!("{:.1}", gib(g.mem_used)),
                unit: "GB".into(),
                frac: Some((g.mem_used as f32 / g.mem_total as f32).clamp(0.0, 1.0)),
                level: None,
            },
            _ => TileText::missing("VRAM"),
        },
        Metric::GpuVramPercent => match &s.gpu {
            Some(g) if g.mem_total > 0 => {
                let percent = (g.mem_used as f32 / g.mem_total as f32 * 100.0).clamp(0.0, 100.0);
                TileText {
                    label: "VRAM".into(),
                    value: format!("{percent:.0}"),
                    unit: "%".into(),
                    frac: Some(percent / 100.0),
                    level: Some(percent),
                }
            }
            _ => TileText::missing("VRAM"),
        },
        Metric::GpuFan => match &s.gpu {
            Some(g) => TileText {
                label: "GPU FAN".into(),
                value: g.fan_percent.map(|value| value.to_string()).unwrap_or_else(|| "—".into()),
                unit: "%".into(),
                frac: g.fan_percent.map(|value| value as f32 / 100.0),
                level: g.fan_percent.map(|value| value as f32),
            },
            None => TileText::missing("GPU FAN"),
        },
        Metric::DiskUsed => match s.disk_percent {
            Some(p) => TileText {
                label: "DISK".into(),
                value: format!("{p:.0}"),
                unit: "%".into(),
                frac: Some((p / 100.0).clamp(0.0, 1.0)),
                level: Some(p),
            },
            None => TileText::missing("DISK"),
        },
        Metric::DiskActivity => match s.disk_activity {
            Some(p) => TileText {
                label: "DISK I/O".into(),
                value: format!("{p:.0}"),
                unit: "%".into(),
                frac: Some((p / 100.0).clamp(0.0, 1.0)),
                level: Some(p),
            },
            None => TileText::missing("DISK I/O"),
        },
        Metric::DiskUsedGb => {
            let used_gb = gib(s.disk_used);
            TileText {
                label: "DISK USED".into(),
                value: format!("{used_gb:.1}"),
                unit: "GB".into(),
                frac: if s.disk_total > 0 { Some(s.disk_used as f32 / s.disk_total as f32) } else { None },
                level: None,
            }
        }
        Metric::NetDown => {
            let (value, unit) = speed_text(s.net_down_bps);
            TileText {
                label: "NET ↓".into(),
                value,
                unit,
                frac: None,
                level: None,
            }
        }
        Metric::NetUp => {
            let (value, unit) = speed_text(s.net_up_bps);
            TileText {
                label: "NET ↑".into(),
                value,
                unit,
                frac: None,
                level: None,
            }
        }
        Metric::Clock => TileText {
            label: "TIME".into(),
            value: if s.clock.is_empty() {
                "—".into()
            } else {
                s.clock.clone()
            },
            unit: String::new(),
            frac: None,
            level: None,
        },
        Metric::Uptime => TileText {
            label: "UPTIME".into(),
            value: uptime_text(s.uptime_secs),
            unit: String::new(),
            frac: None,
            level: None,
        },
    }
}
