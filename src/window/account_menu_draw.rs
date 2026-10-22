//! Owner-drawn account rows for the right-click menu: name plus the same
//! segmented usage bars the taskbar widget shows, for every tracked account.
use super::*;

use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT};

const ODT_MENU: u32 = 1;
const ODS_SELECTED: u32 = 0x1;
const ODS_GRAYED: u32 = 0x2;
const ODS_CHECKED: u32 = 0x8;
/// Tags `itemData` so other owner-drawn items are never mistaken for ours.
const ITEM_TAG: usize = 0xACC0_0000;

#[derive(Clone, Debug)]
pub(super) struct Window {
    pub percentage: f64,
    pub reset: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct AccountRow {
    pub name: String,
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    /// Shown instead of bars when there is no reading.
    pub note: Option<String>,
    pub live: bool,
}

static ROWS: Mutex<Vec<AccountRow>> = Mutex::new(Vec::new());

/// Registers the rows for the menu being opened and returns each row's item data.
pub(super) fn register(rows: Vec<AccountRow>) -> Vec<usize> {
    let mut stored = ROWS.lock().unwrap_or_else(|error| error.into_inner());
    *stored = rows;
    (0..stored.len()).map(|index| ITEM_TAG + index).collect()
}

fn row(item_data: usize) -> Option<AccountRow> {
    let index = item_data.checked_sub(ITEM_TAG)?;
    ROWS.lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(index)
        .cloned()
}

fn scale_for(hwnd: HWND) -> f64 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / 96.0
    }
}

const NAME_WIDTH: f64 = 210.0;
const LABEL_WIDTH: f64 = 24.0;
const BAR_SEGMENT: f64 = 8.0;
const BAR_GAP: f64 = 2.0;
const SEGMENTS: usize = 10;
const PERCENT_WIDTH: f64 = 40.0;
const RESET_WIDTH: f64 = 62.0;
const LEFT_PAD: f64 = 34.0;
const RIGHT_PAD: f64 = 10.0;
const ROW_HEIGHT: f64 = 28.0;

fn bar_width() -> f64 {
    SEGMENTS as f64 * (BAR_SEGMENT + BAR_GAP) - BAR_GAP
}

fn section_width() -> f64 {
    LABEL_WIDTH + bar_width() + 6.0 + PERCENT_WIDTH + RESET_WIDTH + 8.0
}

pub(super) unsafe fn handle_measure(hwnd: HWND, lparam: LPARAM) -> bool {
    let item = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
    if item.CtlType.0 != ODT_MENU || row(item.itemData).is_none() {
        return false;
    }
    let scale = scale_for(hwnd);
    item.itemWidth = ((LEFT_PAD + NAME_WIDTH + 2.0 * section_width() + RIGHT_PAD) * scale) as u32;
    item.itemHeight = (ROW_HEIGHT * scale) as u32;
    true
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

fn channels(color: COLORREF) -> (u8, u8, u8) {
    (color.0 as u8, (color.0 >> 8) as u8, (color.0 >> 16) as u8)
}

fn mix(from: COLORREF, to: COLORREF, amount: f64) -> COLORREF {
    let (a, b) = (channels(from), channels(to));
    let blend = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * amount) as u8;
    rgb(blend(a.0, b.0), blend(a.1, b.1), blend(a.2, b.2))
}

unsafe fn fill(hdc: HDC, left: f64, top: f64, width: f64, height: f64, color: COLORREF) {
    let rect = RECT {
        left: left.round() as i32,
        top: top.round() as i32,
        right: (left + width).round() as i32,
        bottom: (top + height).round() as i32,
    };
    let brush = CreateSolidBrush(color);
    FillRect(hdc, &rect, brush);
    let _ = DeleteObject(brush.into());
}

unsafe fn text(hdc: HDC, value: &str, rect: RECT, flags: DRAW_TEXT_FORMAT, color: COLORREF) {
    let mut wide: Vec<u16> = value.encode_utf16().collect();
    let mut rect = rect;
    SetTextColor(hdc, color);
    DrawTextW(
        hdc,
        &mut wide,
        &mut rect,
        flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
}

pub(super) unsafe fn handle_draw(hwnd: HWND, lparam: LPARAM) -> bool {
    let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
    if item.CtlType.0 != ODT_MENU {
        return false;
    }
    let Some(row) = row(item.itemData) else {
        return false;
    };
    let scale = scale_for(hwnd);
    let hdc = item.hDC;
    let area = item.rcItem;
    let (selected, grayed) = (
        item.itemState.0 & ODS_SELECTED != 0,
        item.itemState.0 & ODS_GRAYED != 0,
    );
    let _ = ODS_CHECKED;

    let background = COLORREF(GetSysColor(COLOR_MENU));
    let foreground = COLORREF(GetSysColor(COLOR_MENUTEXT));
    let (br, bg, bb) = channels(background);
    let dark = u32::from(br) + u32::from(bg) + u32::from(bb) < 3 * 128;
    let row_color = if selected && !grayed {
        mix(background, foreground, 0.08)
    } else {
        background
    };
    let main_text = if grayed {
        mix(background, foreground, 0.5)
    } else {
        foreground
    };
    let muted = mix(background, foreground, 0.55);
    let track = if dark {
        rgb(70, 70, 70)
    } else {
        rgb(208, 208, 208)
    };

    fill(
        hdc,
        f64::from(area.left),
        f64::from(area.top),
        f64::from(area.right - area.left),
        f64::from(area.bottom - area.top),
        row_color,
    );
    SetBkMode(hdc, TRANSPARENT);
    let top = f64::from(area.top);
    let height = f64::from(area.bottom - area.top);
    let rect = |left: f64, width: f64| RECT {
        left: (left * scale) as i32 + area.left,
        top: area.top,
        right: ((left + width) * scale) as i32 + area.left,
        bottom: area.bottom,
    };

    if row.live {
        text(hdc, "\u{2713}", rect(10.0, 20.0), DT_LEFT, main_text);
    }
    text(
        hdc,
        &row.name,
        rect(LEFT_PAD, NAME_WIDTH - 8.0),
        DT_LEFT | DT_END_ELLIPSIS,
        main_text,
    );

    let mut x = LEFT_PAD + NAME_WIDTH;
    if let (Some(note), None, None) = (&row.note, &row.session, &row.weekly) {
        text(hdc, note, rect(x, 2.0 * section_width()), DT_LEFT, muted);
        return true;
    }
    for (label, window) in [("5h", &row.session), ("7d", &row.weekly)] {
        if let Some(window) = window {
            text(hdc, label, rect(x, LABEL_WIDTH), DT_LEFT, muted);
            let bar_left = (x + LABEL_WIDTH) * scale + f64::from(area.left);
            let bar_height = (10.0 * scale).round();
            let bar_top = top + ((height - bar_height) / 2.0).round();
            let used = (window.percentage / 100.0).clamp(0.0, 1.0) * SEGMENTS as f64;
            let color = if window.percentage >= crate::account_switch::WARN_AT_PERCENT {
                rgb(200, 70, 60)
            } else {
                rgb(217, 119, 87)
            };
            for segment in 0..SEGMENTS {
                let left = bar_left + segment as f64 * (BAR_SEGMENT + BAR_GAP) * scale;
                let segment_width = BAR_SEGMENT * scale;
                fill(hdc, left, bar_top, segment_width, bar_height, track);
                let fraction = (used - segment as f64).clamp(0.0, 1.0);
                if fraction > 0.0 {
                    fill(
                        hdc,
                        left,
                        bar_top,
                        segment_width * fraction,
                        bar_height,
                        color,
                    );
                }
            }
            let after_bar = x + LABEL_WIDTH + bar_width() + 6.0;
            text(
                hdc,
                &format!("{:.0}%", window.percentage),
                rect(after_bar, PERCENT_WIDTH),
                DT_LEFT,
                main_text,
            );
            if let Some(reset) = &window.reset {
                text(
                    hdc,
                    reset,
                    rect(after_bar + PERCENT_WIDTH, RESET_WIDTH),
                    DT_LEFT,
                    muted,
                );
            }
        }
        x += section_width();
    }
    true
}
