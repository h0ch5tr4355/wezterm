//! A drop-down menu drawn by the GUI, shown by the `PopupMenu` key assignment.
//!
//! When it is shown right after a status bar link was clicked, it opens
//! underneath that link, like a menu of the macOS menu bar; otherwise it opens
//! underneath the tab bar at the right edge of the window.
//! The choice (or `nil` when the menu was dismissed) is passed to the
//! `wezterm.action_callback` of the assignment.
use crate::overlay::selector::trampoline;
use crate::termwindow::box_model::*;
use crate::termwindow::modal::Modal;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::{
    DimensionContext, GuiWin, TermWindow, TermWindowNotif, UIItem, UIItemType,
};
use crate::utilsprites::RenderMetrics;
use ::window::{
    MouseEvent as WindowMouseEvent, MouseEventKind as WMEK, MousePress, RectF, WindowOps,
};
use config::keyassignment::{
    InputSelectorEntry, KeyAssignment, PopupMenu, PopupMenuEntry, PopupMenuEntryKind,
};
use config::Dimension;
use mux_lua::MuxPane;
use std::cell::{Cell, Ref, RefCell};
use std::io::Read;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::cell::unicode_column_width;
use termwiz::image::{ImageData, ImageDataType};
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent};
use window::color::LinearRgba;

/// Where the status bar link that was clicked last is drawn
#[derive(Clone, Debug, PartialEq)]
pub struct StatusLinkAnchor {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Index of the link in the tab bar state
    pub link: usize,
    pub at: Instant,
}

impl StatusLinkAnchor {
    /// A menu shown later than this after the click was not opened by it
    const FRESH: Duration = Duration::from_secs(5);

    pub fn new(item: &UIItem, link: usize) -> Self {
        Self {
            x: item.x as f32,
            y: item.y as f32,
            width: item.width as f32,
            height: item.height as f32,
            link,
            at: Instant::now(),
        }
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) < Self::FRESH
    }
}

/// Pictures larger than this are not shown
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
/// How often the menu is laid out again while waiting for a picture
const PENDING_RETRY: Duration = Duration::from_millis(50);
/// How long the menu waits for its pictures; after that, it is laid out
/// without the ones that are still missing
const PENDING_LIMIT: Duration = Duration::from_secs(2);
/// Default height of a picture, in lines
const IMAGE_LINES: f32 = 4.;

fn image_lines(entry: &PopupMenuEntry) -> f32 {
    let lines = entry.image_size.unwrap_or(IMAGE_LINES);
    if lines.is_finite() {
        lines.clamp(1., 12.)
    } else {
        IMAGE_LINES
    }
}

/// At most this many pictures are loaded per menu; later entries have none
const MAX_IMAGES: usize = 8;
/// Pictures with a larger width or height (in pixels) are not shown. Menu
/// pictures are small cards; 1024 keeps a decoded frame at 4 MiB
const MAX_IMAGE_DIMENSION: u32 = 1024;

/// Reads at most `max` bytes; None when the source is longer
fn read_limited(reader: impl std::io::Read, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    reader.take(max + 1).read_to_end(&mut data)?;
    Ok(if data.len() as u64 > max {
        None
    } else {
        Some(data)
    })
}

/// Whether the picture size (from its header) is acceptable
fn dimensions_ok(width: u32, height: u32) -> bool {
    width <= MAX_IMAGE_DIMENSION && height <= MAX_IMAGE_DIMENSION
}

/// Reads a picture for an entry; a missing, empty, huge or oversized file
/// gives None, so the entry is shown without it
fn load_image(path: &str) -> Option<Arc<ImageData>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            log::warn!("PopupMenu: cannot open image {path}: {err:#}");
            return None;
        }
    };
    let data = match read_limited(file, MAX_IMAGE_BYTES) {
        Ok(Some(data)) if !data.is_empty() => data,
        Ok(Some(_)) => {
            log::warn!("PopupMenu: skipping empty image {path}");
            return None;
        }
        Ok(None) => {
            log::warn!("PopupMenu: skipping image {path} (over {MAX_IMAGE_BYTES} bytes)");
            return None;
        }
        Err(err) => {
            log::warn!("PopupMenu: cannot read image {path}: {err:#}");
            return None;
        }
    };
    let dims = image::ImageReader::new(std::io::Cursor::new(&data))
        .with_guessed_format()
        .map_err(anyhow::Error::from)
        .and_then(|r| r.into_dimensions().map_err(anyhow::Error::from));
    match dims {
        // Left encoded: the glyph cache decodes it on a background thread
        // (see computed_element), and keeps it by hash, so opening the menu
        // again does not decode the same picture again
        Ok((w, h)) if dimensions_ok(w, h) => Some(Arc::new(ImageData::with_data(
            ImageDataType::EncodedFile(data),
        ))),
        Ok((w, h)) => {
            log::warn!("PopupMenu: skipping image {path} ({w}x{h} pixels is too large)");
            None
        }
        Err(err) => {
            log::warn!("PopupMenu: cannot read the size of image {path}: {err:#}");
            None
        }
    }
}

/// Vertical padding of an item, in cells
const ITEM_PAD_V: f32 = 0.2;
/// Horizontal padding of an item, in cells
const ITEM_PAD_H: f32 = 0.6;
/// Padding inside the menu frame, in cells
const FRAME_PAD: f32 = 0.3;
/// Height of a separator including its spacing, in cells
const SEPARATOR_HEIGHT: f32 = 0.7;
/// Width of a progress bar, in cells
const BAR_CELLS: f32 = 12.;
/// Distance from the window edges, in pixels
const EDGE_GAP: f32 = 6.;

/// What a mouse event does while the menu is open
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseOutcome {
    /// Not for the menu; let the rest of the window handle it
    Ignore,
    /// For the menu, but nothing happens
    Consume,
    /// Highlight this item (or none); `consumed` = the pointer is over the menu
    Hover {
        item: Option<usize>,
        consumed: bool,
    },
    Choose(usize),
    /// Close the menu; with `pass_through` the click then reaches what is
    /// underneath, so that clicking another status link opens its menu
    Dismiss {
        pass_through: bool,
    },
    Scroll(isize),
}

/// `hit`: `None` = the pointer is outside the menu, `Some(None)` = over the
/// menu but not over an item that can be chosen.
fn decide(
    kind: &WMEK,
    hit: Option<Option<usize>>,
    over_link: Option<usize>,
    origin: Option<usize>,
    streak: usize,
) -> MouseOutcome {
    match (kind, hit) {
        (WMEK::Move, Some(item)) => MouseOutcome::Hover {
            item,
            consumed: true,
        },
        (WMEK::Move, None) => MouseOutcome::Hover {
            item: None,
            consumed: false,
        },
        (WMEK::Press(MousePress::Left), Some(Some(item))) => MouseOutcome::Choose(item),
        (WMEK::Press(_), Some(_)) => MouseOutcome::Consume,
        (WMEK::Press(MousePress::Left), None) => {
            if over_link.is_some() && over_link == origin {
                if streak > 1 {
                    // The second click of a double click on the link that
                    // opened the menu: keep it open
                    MouseOutcome::Consume
                } else {
                    // Clicking the link again closes its menu
                    MouseOutcome::Dismiss {
                        pass_through: false,
                    }
                }
            } else {
                MouseOutcome::Dismiss {
                    pass_through: over_link.is_some(),
                }
            }
        }
        (WMEK::Press(_), None) => MouseOutcome::Dismiss {
            pass_through: false,
        },
        (WMEK::Release(_), Some(_)) => MouseOutcome::Consume,
        (WMEK::Release(_), None) => MouseOutcome::Ignore,
        (WMEK::VertWheel(n), Some(_)) => MouseOutcome::Scroll(if *n > 0 { -1 } else { 1 }),
        // While the menu is open, the terminal underneath does not scroll
        (WMEK::VertWheel(_), None) => MouseOutcome::Consume,
        (WMEK::HorzWheel(_), Some(_)) => MouseOutcome::Consume,
        (WMEK::HorzWheel(_), None) => MouseOutcome::Ignore,
    }
}

/// The next item that can be chosen in direction `dir` (+1/-1), wrapping
/// around; from no item, Down starts at the top and Up at the bottom.
fn next_selectable(entries: &[PopupMenuEntry], from: Option<usize>, dir: isize) -> Option<usize> {
    let n = entries.len() as isize;
    if n == 0 {
        return None;
    }
    let start = match from {
        Some(i) => i as isize,
        None if dir > 0 => -1,
        None => n,
    };
    let mut idx = start;
    for _ in 0..n {
        idx = (idx + dir).rem_euclid(n);
        if entries[idx as usize].is_selectable() {
            return Some(idx as usize);
        }
    }
    None
}

fn first_selectable(entries: &[PopupMenuEntry], dir: isize) -> Option<usize> {
    next_selectable(entries, None, dir)
}

/// Top left corner of the menu. Underneath the anchor, aligned with its left
/// edge, shifted left so that the menu stays inside the window.
fn place(
    anchor: Option<&StatusLinkAnchor>,
    menu_width: f32,
    window_width: f32,
    top: f32,
) -> (f32, f32) {
    let max_x = (window_width - menu_width - EDGE_GAP).max(0.);
    match anchor {
        Some(a) => (a.x.min(max_x).max(0.), (a.y + a.height).max(top) + 2.),
        None => (max_x, top + 2.),
    }
}

/// How many entries from `top` fit into `available` pixels
fn fit_count(heights: &[f32], top: usize, available: f32) -> usize {
    let mut used = 0.;
    let mut count = 0;
    for h in heights.iter().skip(top) {
        if used + h > available && count > 0 {
            break;
        }
        used += h;
        count += 1;
    }
    count
}

/// The first visible entry, moved as little as possible so that `selected`
/// is visible and the menu shows as many entries as fit
fn scroll_to_show(heights: &[f32], top: usize, selected: Option<usize>, available: f32) -> usize {
    let mut top = top.min(heights.len().saturating_sub(1));
    // Fill the space at the bottom when the menu was scrolled too far
    while top > 0 && fit_count(heights, top - 1, available) > fit_count(heights, top, available) {
        top -= 1;
    }
    if let Some(sel) = selected {
        if sel < top {
            top = sel;
        }
        while top < sel && sel >= top + fit_count(heights, top, available) {
            top += 1;
        }
    }
    top
}

/// Width of the text after the progress bars, in cells: the widest one, so
/// that the bars of all rows end at the same place. Cards (entries with a
/// picture, `is_card`) draw their bar differently and do not count.
fn bar_detail_cells(entries: &[PopupMenuEntry], is_card: impl Fn(usize) -> bool) -> f32 {
    entries
        .iter()
        .enumerate()
        .filter(|(idx, e)| e.progress.is_some() && !is_card(*idx))
        .filter_map(|(_, e)| e.detail.as_deref())
        .map(|d| unicode_column_width(d, None) as f32)
        .fold(4., f32::max)
}

/// A bar `total` pixels wide, filled to `fraction`
fn progress_bar(
    font: &Rc<LoadedFont>,
    fraction: f32,
    total: f32,
    color: LinearRgba,
    track: LinearRgba,
) -> Element {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0., 1.)
    } else {
        0.
    };
    let filled = (total * fraction).round();
    let segment = |width: f32, color: LinearRgba| {
        Element::new(font, ElementContent::Text(String::new()))
            .min_width(Some(Dimension::Pixels(width)))
            .line_height(Some(0.3))
            .colors(ElementColors {
                border: BorderColor::default(),
                bg: color.into(),
                text: color.into(),
            })
    };
    let mut parts = vec![];
    if filled > 0. {
        parts.push(segment(filled, color));
    }
    if total - filled > 0. {
        parts.push(segment(total - filled, track));
    }
    Element::new(font, ElementContent::Children(parts))
}

fn badge_element(
    font: &Rc<LoadedFont>,
    text: &str,
    color: Option<config::RgbaColor>,
    pal: &Palette,
) -> Element {
    let bg = color.map(|c| c.to_linear()).unwrap_or(pal.dim);
    Element::new(font, ElementContent::Text(text.to_string()))
        .margin(sides(cells(0.6), cells(0.), cells(0.), cells(0.)))
        .padding(sides(cells(0.4), cells(0.4), cells(0.), cells(0.)))
        .border(BoxDimension::new(Dimension::Pixels(1.)))
        .border_corners(Some(rounded(cells(0.3))))
        .vertical_align(VerticalAlign::Middle)
        .colors(ElementColors {
            border: BorderColor::new(bg),
            bg: bg.into(),
            text: pal.bg.into(),
        })
}

/// The keys of a shortcut: "Ctrl+Shift+T" gives "Ctrl", "Shift", "T".
/// Text that does not split cleanly (eg: "+" or "Ctrl++") stays one key.
fn split_keys(text: &str) -> Vec<&str> {
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    if parts.len() > 1 && parts.iter().all(|p| !p.is_empty()) {
        parts
    } else {
        vec![text.trim()]
    }
}

/// A shortcut as a row of key caps
fn keys_element(font: &Rc<LoadedFont>, text: &str, enabled: bool, pal: &Palette) -> Element {
    let cap_bg = mix(pal.bg, pal.fg, 0.1);
    let cap_border = mix(pal.bg, pal.fg, 0.28);
    let cap_text = if enabled { pal.fg } else { pal.dim };
    let caps = split_keys(text)
        .into_iter()
        .enumerate()
        .map(|(i, key)| {
            Element::new(font, ElementContent::Text(key.to_string()))
                .margin(sides(
                    cells(if i == 0 { 0. } else { 0.25 }),
                    cells(0.),
                    cells(0.),
                    cells(0.),
                ))
                .padding(sides(cells(0.45), cells(0.45), cells(0.), cells(0.)))
                .border(BoxDimension::new(Dimension::Pixels(1.)))
                .border_corners(Some(rounded(cells(0.3))))
                .vertical_align(VerticalAlign::Middle)
                .colors(ElementColors {
                    border: BorderColor::new(cap_border),
                    bg: cap_bg.into(),
                    text: cap_text.into(),
                })
        })
        .collect();
    Element::new(font, ElementContent::Children(caps))
        .margin(sides(cells(1.), cells(0.), cells(0.), cells(0.)))
        .vertical_align(VerticalAlign::Middle)
}

/// The frame of an item: padding, the rounded highlight and the hit area
fn item_row(
    font: &Rc<LoadedFont>,
    kids: Vec<Element>,
    idx: usize,
    width: Option<Dimension>,
    selected: bool,
    enabled: bool,
    pal: &Palette,
) -> Element {
    let mut row = Element::new(font, ElementContent::Children(kids))
        .display(DisplayType::Block)
        .padding(sides(
            cells(ITEM_PAD_H),
            cells(ITEM_PAD_H),
            cells(ITEM_PAD_V),
            cells(ITEM_PAD_V),
        ))
        .border(BoxDimension::new(Dimension::Pixels(1.)))
        .border_corners(Some(rounded(cells(0.3))))
        .min_width(width)
        .colors(if selected {
            ElementColors {
                border: BorderColor::new(pal.highlight),
                bg: pal.highlight.into(),
                text: pal.fg.into(),
            }
        } else {
            ElementColors {
                border: BorderColor::default(),
                bg: LinearRgba::TRANSPARENT.into(),
                text: pal.fg.into(),
            }
        });
    if enabled {
        row = row.item_type(UIItemType::PopupMenu(Some(idx)));
    }
    row
}

fn rounded(radius: Dimension) -> Corners {
    Corners {
        top_left: SizedPoly {
            width: radius,
            height: radius,
            poly: TOP_LEFT_ROUNDED_CORNER,
        },
        top_right: SizedPoly {
            width: radius,
            height: radius,
            poly: TOP_RIGHT_ROUNDED_CORNER,
        },
        bottom_left: SizedPoly {
            width: radius,
            height: radius,
            poly: BOTTOM_LEFT_ROUNDED_CORNER,
        },
        bottom_right: SizedPoly {
            width: radius,
            height: radius,
            poly: BOTTOM_RIGHT_ROUNDED_CORNER,
        },
    }
}

fn cells(n: f32) -> Dimension {
    Dimension::Cells(n)
}

fn sides(left: Dimension, right: Dimension, top: Dimension, bottom: Dimension) -> BoxDimension {
    BoxDimension {
        left,
        right,
        top,
        bottom,
    }
}

fn plain(text: LinearRgba) -> ElementColors {
    ElementColors {
        border: BorderColor::default(),
        bg: LinearRgba::TRANSPARENT.into(),
        text: text.into(),
    }
}

/// `from` moved towards `to` by `amount` (0.0 to 1.0), opaque. Glyphs are
/// drawn without the alpha of their color, so dimmed text needs a real color.
fn mix(from: LinearRgba, to: LinearRgba, amount: f32) -> LinearRgba {
    let lerp = |a: f32, b: f32| a + (b - a) * amount;
    LinearRgba::with_components(
        lerp(from.0, to.0),
        lerp(from.1, to.1),
        lerp(from.2, to.2),
        1.0,
    )
}

struct Palette {
    bg: LinearRgba,
    fg: LinearRgba,
    dim: LinearRgba,
    highlight: LinearRgba,
    border: LinearRgba,
}

pub struct PopupMenuModal {
    args: PopupMenu,
    event_name: String,
    window: GuiWin,
    pane: MuxPane,
    anchor: Option<StatusLinkAnchor>,
    /// Decoded pictures of the entries, by index
    images: Vec<Option<Arc<ImageData>>>,
    selected: RefCell<Option<usize>>,
    top: RefCell<usize>,
    element: RefCell<Option<Vec<ComputedElement>>>,
    /// A layout with a picture that is still being decoded. Paints reuse
    /// it until the retry timer asks for a new one.
    pending: RefCell<Option<Vec<ComputedElement>>>,
    /// When the menu started to wait for its pictures
    pending_since: Cell<Option<Instant>>,
    /// A retry timer is running
    retry_armed: Cell<bool>,
    /// The natural width of the last layout and what it depends on. The
    /// highlight does not change it, so moving over the items reuses it
    /// instead of laying the menu out twice.
    natural_width: Cell<Option<(NaturalWidthKey, f32)>>,
    /// Width of the widest label that is followed by `keys`, so that the
    /// shortcuts line up, and the cell size it was measured with
    keys_column: Cell<Option<((f32, f32), Option<f32>)>>,
}

/// What the natural width of the menu depends on, apart from the entries
/// (which do not change while it is open)
#[derive(Clone, Copy, Debug, PartialEq)]
struct NaturalWidthKey {
    first: usize,
    count: usize,
    window_width: f32,
    window_height: f32,
    cell_width: f32,
    cell_height: f32,
}

impl PopupMenuModal {
    pub fn new(
        term_window: &TermWindow,
        args: PopupMenu,
        pane: MuxPane,
        anchor: Option<StatusLinkAnchor>,
    ) -> anyhow::Result<Self> {
        let event_name = match *args.action {
            KeyAssignment::EmitEvent(ref id) => id.to_string(),
            _ => {
                anyhow::bail!("PopupMenu requires action to be defined by wezterm.action_callback")
            }
        };
        // Only items show a picture (see entry_height)
        let mut loaded = 0;
        let images = args
            .entries
            .iter()
            .map(|e| {
                let path = e.image.as_deref()?;
                if !matches!(e.kind, PopupMenuEntryKind::Item) {
                    return None;
                }
                if loaded >= MAX_IMAGES {
                    log::warn!("PopupMenu: more than {MAX_IMAGES} images, skipping {path}");
                    return None;
                }
                loaded += 1;
                load_image(path)
            })
            .collect();
        Ok(Self {
            images,
            args,
            event_name,
            window: GuiWin::new(term_window),
            pane,
            anchor,
            selected: RefCell::new(None),
            top: RefCell::new(0),
            element: RefCell::new(None),
            pending: RefCell::new(None),
            pending_since: Cell::new(None),
            retry_armed: Cell::new(false),
            natural_width: Cell::new(None),
            keys_column: Cell::new(None),
        })
    }

    /// The link that opened the menu, if any
    pub fn origin(&self) -> Option<usize> {
        self.anchor.as_ref().map(|a| a.link)
    }

    fn finish(&self, term_window: &mut TermWindow, chosen: Option<usize>) {
        term_window.cancel_modal();
        let entry = chosen
            .and_then(|idx| self.args.entries.get(idx))
            .map(|e| InputSelectorEntry {
                label: e.label.clone(),
                id: e.id.clone(),
            });
        trampoline(
            self.event_name.clone(),
            self.window.clone(),
            self.pane.clone(),
            entry,
        );
    }

    fn select(&self, term_window: &mut TermWindow, item: Option<usize>) {
        if *self.selected.borrow() != item {
            *self.selected.borrow_mut() = item;
            term_window.invalidate_modal();
        }
    }

    /// Handles a mouse event while the menu is open. `hit` is the item under
    /// the pointer (see [decide]), `over_link` the status link under it.
    /// Returns true when the event must not reach the rest of the window.
    pub fn window_mouse_event(
        &self,
        event: &WindowMouseEvent,
        hit: Option<Option<usize>>,
        over_link: Option<usize>,
        streak: usize,
        term_window: &mut TermWindow,
    ) -> bool {
        let hit = hit.map(|item| item.filter(|&i| self.selectable(i)));
        match decide(&event.kind, hit, over_link, self.origin(), streak) {
            MouseOutcome::Ignore => false,
            MouseOutcome::Consume => true,
            MouseOutcome::Hover { item, consumed } => {
                self.select(term_window, item);
                consumed
            }
            MouseOutcome::Choose(item) => {
                self.finish(term_window, Some(item));
                true
            }
            MouseOutcome::Dismiss { pass_through } => {
                self.finish(term_window, None);
                !pass_through
            }
            MouseOutcome::Scroll(delta) => {
                let top = *self.top.borrow();
                *self.top.borrow_mut() = (top as isize + delta).max(0) as usize;
                // Otherwise the highlighted item would be kept in view
                *self.selected.borrow_mut() = None;
                term_window.invalidate_modal();
                true
            }
        }
    }

    fn selectable(&self, idx: usize) -> bool {
        self.args
            .entries
            .get(idx)
            .map_or(false, PopupMenuEntry::is_selectable)
    }

    fn palette(&self, term_window: &TermWindow) -> Palette {
        let config = &term_window.config;
        let bg = self
            .args
            .bg_color
            .unwrap_or(config.command_palette_bg_color)
            .to_linear();
        let fg = self
            .args
            .fg_color
            .unwrap_or(config.command_palette_fg_color)
            .to_linear();
        let pick = |c: Option<config::RgbaColor>, fallback: LinearRgba| {
            c.map(|c| c.to_linear()).unwrap_or(fallback)
        };
        Palette {
            bg,
            fg,
            dim: pick(self.args.dim_color, mix(fg, bg, 0.45)),
            highlight: pick(self.args.highlight_color, mix(bg, fg, 0.16)),
            border: pick(self.args.border_color, mix(bg, fg, 0.22)),
        }
    }

    fn has_image(&self, idx: usize) -> bool {
        matches!(self.images.get(idx), Some(Some(_)))
    }

    fn entry_height(&self, idx: usize, entry: &PopupMenuEntry, cell_height: f32) -> f32 {
        match entry.kind {
            PopupMenuEntryKind::Separator => cell_height * SEPARATOR_HEIGHT,
            PopupMenuEntryKind::Item if self.has_image(idx) => {
                let text_lines = 1.
                    + entry.detail.as_deref().map_or(0, |d| d.lines().count()) as f32
                    + if entry.progress.is_some() { 1. } else { 0. };
                cell_height * (image_lines(entry).max(text_lines) + 2. * ITEM_PAD_V) + 2.
            }
            PopupMenuEntryKind::Item | PopupMenuEntryKind::Header => {
                cell_height * (1. + 2. * ITEM_PAD_V) + 2.
            }
        }
    }

    /// One row of the menu. `row_width` is the content width of an item
    /// (None while measuring the natural width). Labels followed by `keys`
    /// are `keys_column` pixels wide.
    #[allow(clippy::too_many_arguments)]
    fn entry_element(
        &self,
        font: &Rc<LoadedFont>,
        header_font: &Rc<LoadedFont>,
        idx: usize,
        entry: &PopupMenuEntry,
        with_icons: bool,
        row_width: Option<f32>,
        cell_width: f32,
        bar_detail_cells: f32,
        keys_column: Option<f32>,
        pal: &Palette,
    ) -> Element {
        let full = row_width.map(Dimension::Pixels);

        if entry.kind == PopupMenuEntryKind::Separator {
            let mut border = BorderColor::default();
            border.bottom = pal.border;
            let line_width = row_width.map(|w| w + 2. * ITEM_PAD_H * cell_width);
            return Element::new(font, ElementContent::Text(String::new()))
                .display(DisplayType::Block)
                .line_height(Some((SEPARATOR_HEIGHT / 2.) as f64))
                .min_width(line_width.map(Dimension::Pixels))
                .border(sides(
                    Dimension::Pixels(0.),
                    Dimension::Pixels(0.),
                    Dimension::Pixels(0.),
                    Dimension::Pixels(1.),
                ))
                .margin(sides(
                    Dimension::Pixels(0.),
                    Dimension::Pixels(0.),
                    Dimension::Pixels(0.),
                    cells(SEPARATOR_HEIGHT / 2. - 0.05),
                ))
                .colors(ElementColors {
                    border,
                    bg: LinearRgba::TRANSPARENT.into(),
                    text: pal.border.into(),
                });
        }

        let selected = *self.selected.borrow() == Some(idx) && entry.is_selectable();
        let enabled = entry.kind == PopupMenuEntryKind::Item && entry.id.is_some();
        // An explicit color wins, so that rows that only show information
        // need not look disabled
        let label_color = match (entry.kind, entry.color) {
            (PopupMenuEntryKind::Header, None) => pal.dim,
            (_, Some(color)) => color.to_linear(),
            _ if !enabled => pal.dim,
            _ => pal.fg,
        };

        let mut kids = vec![];
        if with_icons && entry.kind == PopupMenuEntryKind::Item {
            let icon_color = entry
                .icon_color
                .map(|c| c.to_linear())
                .unwrap_or(label_color);
            kids.push(
                Element::new(
                    font,
                    ElementContent::Text(entry.icon.clone().unwrap_or_default()),
                )
                .min_width(Some(cells(2.)))
                .colors(plain(icon_color)),
            );
        }
        let label_font = if entry.kind == PopupMenuEntryKind::Header {
            header_font
        } else {
            font
        };
        let mut label = Element::new(label_font, ElementContent::Text(entry.label.clone()))
            .vertical_align(VerticalAlign::Bottom)
            .colors(plain(label_color));
        if entry.keys.is_some() && entry.kind == PopupMenuEntryKind::Item {
            label = label.min_width(keys_column.map(Dimension::Pixels));
        }
        kids.push(label);

        if entry.kind == PopupMenuEntryKind::Item {
            if let Some(keys) = &entry.keys {
                kids.push(keys_element(font, keys, enabled, pal));
            }
        }
        if let Some(badge) = &entry.badge {
            kids.push(badge_element(font, badge, entry.badge_color, pal));
        }

        let bar_color = entry
            .progress_color
            .map(|c| c.to_linear())
            .unwrap_or(pal.fg);

        if let Some(image) = self.images.get(idx).cloned().flatten() {
            return self.card_element(
                font,
                idx,
                entry,
                image,
                entry.progress,
                label_color,
                bar_color,
                row_width,
                cell_width,
                selected,
                enabled,
                pal,
            );
        }

        if let Some(fraction) = entry.progress {
            let bar = progress_bar(
                font,
                fraction,
                BAR_CELLS * cell_width,
                bar_color,
                pal.border,
            )
            .vertical_align(VerticalAlign::Middle);
            // The bar goes to the right edge together with the detail, so
            // that the bars of several rows line up
            let detail_color = entry.detail_color.map(|c| c.to_linear()).unwrap_or(pal.dim);
            let detail = Element::new(
                font,
                ElementContent::Text(entry.detail.clone().unwrap_or_default()),
            )
            .min_width(Some(cells(bar_detail_cells)))
            .padding(sides(cells(0.8), cells(0.), cells(0.), cells(0.)))
            .colors(plain(detail_color));
            kids.push(
                Element::new(font, ElementContent::Children(vec![bar, detail]))
                    .float(Float::Right)
                    .margin(sides(cells(2.), cells(0.), cells(0.), cells(0.))),
            );
        } else if let Some(detail) = &entry.detail {
            let detail_color = entry.detail_color.map(|c| c.to_linear()).unwrap_or(pal.dim);
            kids.push(
                Element::new(font, ElementContent::Text(detail.clone()))
                    .float(Float::Right)
                    .margin(sides(cells(2.), cells(0.), cells(0.), cells(0.)))
                    .colors(plain(detail_color)),
            );
        }

        item_row(font, kids, idx, full, selected, enabled, pal)
    }

    /// An item with a picture: the picture at the left, the label, each
    /// line of the detail and the progress bar stacked next to it
    #[allow(clippy::too_many_arguments)]
    fn card_element(
        &self,
        font: &Rc<LoadedFont>,
        idx: usize,
        entry: &PopupMenuEntry,
        image: Arc<ImageData>,
        progress: Option<f32>,
        label_color: LinearRgba,
        bar_color: LinearRgba,
        row_width: Option<f32>,
        cell_width: f32,
        selected: bool,
        enabled: bool,
        pal: &Palette,
    ) -> Element {
        let side = Dimension::Cells(0.);
        let lines = image_lines(entry);
        let picture = Element::new(
            font,
            ElementContent::Image {
                data: image,
                // Lines are the height of a cell; the picture is square
                width: Dimension::Pixels(lines * font.metrics().cell_height.get() as f32),
                height: Dimension::Pixels(lines * font.metrics().cell_height.get() as f32),
            },
        )
        .margin(sides(side, cells(0.9), side, side))
        .vertical_align(VerticalAlign::Middle);

        let mut first_line = vec![
            Element::new(font, ElementContent::Text(entry.label.clone()))
                .colors(plain(label_color)),
        ];
        if let Some(keys) = &entry.keys {
            first_line.push(keys_element(font, keys, enabled, pal));
        }
        if let Some(badge) = &entry.badge {
            first_line.push(badge_element(font, badge, entry.badge_color, pal));
        }
        let mut text =
            vec![Element::new(font, ElementContent::Children(first_line))
                .display(DisplayType::Block)];
        let detail_color = entry.detail_color.map(|c| c.to_linear()).unwrap_or(pal.dim);
        for line in entry.detail.as_deref().unwrap_or("").lines() {
            text.push(
                Element::new(font, ElementContent::Text(line.to_string()))
                    .display(DisplayType::Block)
                    .colors(plain(detail_color)),
            );
        }
        if let Some(fraction) = progress {
            text.push(
                Element::new(
                    font,
                    ElementContent::Children(vec![progress_bar(
                        font,
                        fraction,
                        BAR_CELLS * 1.5 * cell_width,
                        bar_color,
                        pal.border,
                    )]),
                )
                .display(DisplayType::Block)
                .padding(sides(side, side, cells(0.35), side)),
            );
        }
        let kids = vec![
            picture,
            Element::new(font, ElementContent::Children(text))
                .vertical_align(VerticalAlign::Middle),
        ];
        let full = row_width.map(Dimension::Pixels);
        item_row(font, kids, idx, full, selected, enabled, pal)
    }

    fn compute(&self, term_window: &mut TermWindow) -> anyhow::Result<Vec<ComputedElement>> {
        let font = term_window.fonts.command_palette_font()?;
        let header_font = term_window.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cell_width = metrics.cell_size.width as f32;
        let cell_height = metrics.cell_size.height as f32;
        let pal = self.palette(term_window);

        let dimensions = term_window.dimensions;
        let window_width = dimensions.pixel_width as f32;
        let window_height = dimensions.pixel_height as f32;
        let border = term_window.get_os_border();
        let top = if term_window.show_tab_bar && !term_window.config.tab_bar_at_bottom {
            term_window.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        } + border.top.get() as f32;
        let bottom_limit = window_height
            - border.bottom.get() as f32
            - if term_window.show_tab_bar && term_window.config.tab_bar_at_bottom {
                term_window.tab_bar_pixel_height().unwrap_or(0.)
            } else {
                0.
            };

        let entries = &self.args.entries;
        let heights: Vec<f32> = entries
            .iter()
            .enumerate()
            .map(|(idx, e)| self.entry_height(idx, e, cell_height))
            .collect();
        let frame = 2. * (FRAME_PAD * cell_height + 1.);
        let anchor_bottom = self
            .anchor
            .as_ref()
            .map_or(top, |a| (a.y + a.height).max(top));
        let available = (bottom_limit - anchor_bottom - EDGE_GAP - frame).max(cell_height);

        // When not everything fits, arrows above and below show that the
        // menu scrolls; they take space from the entries
        let arrow_height = cell_height * (1. + 2. * ITEM_PAD_V) + 2.;
        let total: f32 = heights.iter().sum();
        let available = if total > available {
            (available - 2. * arrow_height).max(cell_height)
        } else {
            available
        };
        let first = scroll_to_show(
            &heights,
            *self.top.borrow(),
            *self.selected.borrow(),
            available,
        );
        *self.top.borrow_mut() = first;
        let count = fit_count(&heights, first, available);
        let more_above = first > 0;
        let more_below = first + count < entries.len();
        let with_icons = entries.iter().any(|e| e.icon.is_some());
        let bar_detail_cells = bar_detail_cells(entries, |idx| self.has_image(idx));

        let context = |bounds: RectF| LayoutContext {
            height: DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: window_height,
                pixel_cell: cell_height,
            },
            width: DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: window_width,
                pixel_cell: cell_width,
            },
            bounds,
            metrics: &metrics,
            gl_state: term_window.render_state.as_ref().unwrap(),
            zindex: 100,
        };

        // The shortcuts start in one column: after the widest label that
        // has one. Cards draw them on their own line and do not count.
        let keys_column = match self.keys_column.get() {
            Some((size, column)) if size == (cell_width, cell_height) => column,
            _ => {
                let mut column: Option<f32> = None;
                for (idx, e) in entries.iter().enumerate() {
                    if e.kind != PopupMenuEntryKind::Item || e.keys.is_none() || self.has_image(idx)
                    {
                        continue;
                    }
                    let label = term_window.compute_element(
                        &context(euclid::rect(0., 0., window_width, window_height)),
                        &Element::new(&font, ElementContent::Text(e.label.clone())),
                    )?;
                    column = Some(column.unwrap_or(0.).max(label.bounds.width()));
                }
                self.keys_column
                    .set(Some(((cell_width, cell_height), column)));
                column
            }
        };

        let arrow = |text: &str, show: bool| {
            Element::new(
                &font,
                ElementContent::Text(if show { text } else { "" }.to_string()),
            )
            .display(DisplayType::Block)
            .min_height(Some(Dimension::Pixels(arrow_height)))
            .padding(sides(cells(ITEM_PAD_H), cells(0.), cells(0.), cells(0.)))
            .colors(plain(pal.dim))
        };
        let build = |row_width: Option<f32>| -> Element {
            let mut rows: Vec<Element> = vec![];
            if more_above {
                rows.push(arrow("▲", true));
            }
            rows.extend(
                entries
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(count)
                    .map(|(idx, e)| {
                        self.entry_element(
                            &font,
                            &header_font,
                            idx,
                            e,
                            with_icons,
                            row_width,
                            cell_width,
                            bar_detail_cells,
                            keys_column,
                            &pal,
                        )
                    }),
            );
            if more_above || more_below {
                rows.push(arrow("▼", more_below));
            }
            let radius = cells(0.5);
            let inner = Element::new(&font, ElementContent::Children(rows))
                .padding(BoxDimension::new(cells(FRAME_PAD)))
                .border(BoxDimension::new(Dimension::Pixels(1.)))
                .border_corners(Some(rounded(radius)))
                .colors(ElementColors {
                    border: BorderColor::new(pal.bg),
                    bg: pal.bg.into(),
                    text: pal.fg.into(),
                });
            // The outer box draws the frame: rounded corners are filled with
            // the border color, so a separate inner box keeps them crisp
            Element::new(&font, ElementContent::Children(vec![inner]))
                .padding(BoxDimension::new(Dimension::Pixels(1.)))
                .border(BoxDimension::new(Dimension::Pixels(1.)))
                .border_corners(Some(rounded(radius)))
                .item_type(UIItemType::PopupMenu(None))
                .colors(ElementColors {
                    border: BorderColor::new(pal.border),
                    bg: pal.border.into(),
                    text: pal.fg.into(),
                })
        };

        // Measure the natural width first, then lay out again with every row
        // as wide as the widest one so that highlights line up
        let max_width = (window_width - 2. * EDGE_GAP).max(cell_width);
        let key = NaturalWidthKey {
            first,
            count,
            window_width,
            window_height,
            cell_width,
            cell_height,
        };
        let natural_width = match self.natural_width.get() {
            Some((cached, width)) if cached == key => width,
            _ => {
                let natural = term_window.compute_element(
                    &context(euclid::rect(0., 0., max_width, window_height)),
                    &build(None),
                )?;
                let width = natural.bounds.width();
                self.natural_width.set(Some((key, width)));
                width
            }
        };
        let min_width = self
            .args
            .min_width
            .map_or(0., |cols| cols as f32 * cell_width);
        let width = natural_width.max(min_width).min(max_width);
        // frame: 2 x (1px border + 1px padding) outside, inner border and padding,
        // item border and padding
        let row_width =
            (width - 4. - 2. * (1. + FRAME_PAD * cell_width) - 2. * (1. + ITEM_PAD_H * cell_width))
                .max(0.);

        let (x, y) = place(self.anchor.as_ref(), width, window_width, top);
        let menu = term_window.compute_element(
            &context(euclid::rect(x, y, width, bottom_limit - y)),
            &build(Some(row_width)),
        )?;

        let shadow_color = LinearRgba::with_components(0., 0., 0., 0.3);
        let shadow = Element::new(&font, ElementContent::Text(String::new()))
            .min_width(Some(Dimension::Pixels(menu.bounds.width())))
            .min_height(Some(Dimension::Pixels(menu.bounds.height())))
            .border_corners(Some(rounded(cells(0.5))))
            .colors(ElementColors {
                border: BorderColor::new(shadow_color),
                bg: shadow_color.into(),
                text: shadow_color.into(),
            });
        let mut shadow_context = context(euclid::rect(
            menu.bounds.min_x() + 2.,
            menu.bounds.min_y() + 4.,
            menu.bounds.width(),
            menu.bounds.height(),
        ));
        shadow_context.zindex = 99;
        let shadow = term_window.compute_element(&shadow_context, &shadow)?;

        Ok(vec![shadow, menu])
    }
}

impl Modal for PopupMenuModal {
    fn mouse_event(&self, _event: MouseEvent, _term_window: &mut TermWindow) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        let entries = &self.args.entries;
        let current = *self.selected.borrow();
        match (key, mods) {
            (KeyCode::Escape, _) | (KeyCode::Char('g'), KeyModifiers::CTRL) => {
                self.finish(term_window, None);
            }
            (KeyCode::UpArrow, _) | (KeyCode::Char('p'), KeyModifiers::CTRL) => {
                self.select(term_window, next_selectable(entries, current, -1));
            }
            (KeyCode::DownArrow, _)
            | (KeyCode::Tab, _)
            | (KeyCode::Char('n'), KeyModifiers::CTRL) => {
                self.select(term_window, next_selectable(entries, current, 1));
            }
            (KeyCode::Home, _) | (KeyCode::PageUp, _) => {
                self.select(term_window, first_selectable(entries, 1));
            }
            (KeyCode::End, _) | (KeyCode::PageDown, _) => {
                self.select(term_window, first_selectable(entries, -1));
            }
            (KeyCode::Enter, _) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
                if let Some(idx) = current.filter(|&i| self.selectable(i)) {
                    self.finish(term_window, Some(idx));
                }
            }
            // Like a menu of the operating system, the menu takes all keys
            _ => {}
        }
        Ok(true)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.element.borrow().is_none() {
            // Paints caused by something else (terminal output, a spinner)
            // reuse the layout that waits for a picture; only the retry
            // timer below asks for a new one
            if self.pending.borrow().is_some() {
                return Ok(Ref::map(self.pending.borrow(), |v| {
                    v.as_ref().unwrap().as_slice()
                }));
            }
            let element = self.compute(term_window)?;
            let now = Instant::now();
            let since = self.pending_since.get().unwrap_or(now);
            // Limited by time, not by the number of paints: a picture that
            // is never decoded must not keep the menu waiting forever
            if element.iter().any(|e| e.has_pending_image())
                && now.saturating_duration_since(since) < PENDING_LIMIT
            {
                self.pending_since.set(Some(since));
                if !self.retry_armed.replace(true) {
                    if let Some(window) = term_window.window.clone() {
                        promise::spawn::spawn(async move {
                            smol::Timer::after(PENDING_RETRY).await;
                            window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                                if let Some(modal) = term_window.get_modal() {
                                    if let Some(menu) = modal.downcast_ref::<PopupMenuModal>() {
                                        menu.retry_armed.set(false);
                                    }
                                }
                                // Drops the waiting layout and repaints
                                term_window.invalidate_modal();
                            })));
                        })
                        .detach();
                    }
                }
                *self.pending.borrow_mut() = Some(element);
                return Ok(Ref::map(self.pending.borrow(), |v| {
                    v.as_ref().unwrap().as_slice()
                }));
            }
            self.element.borrow_mut().replace(element);
        }
        Ok(Ref::map(self.element.borrow(), |v| {
            v.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
        self.pending.borrow_mut().take();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn item(id: Option<&str>) -> PopupMenuEntry {
        PopupMenuEntry {
            label: "x".into(),
            id: id.map(Into::into),
            ..Default::default()
        }
    }

    fn kind(kind: PopupMenuEntryKind) -> PopupMenuEntry {
        PopupMenuEntry {
            kind,
            id: Some("ignored".into()),
            ..Default::default()
        }
    }

    #[test]
    fn shortcuts_split_into_keys() {
        assert_eq!(split_keys("Ctrl+Shift+T"), vec!["Ctrl", "Shift", "T"]);
        assert_eq!(split_keys("Alt+Shift+-"), vec!["Alt", "Shift", "-"]);
        assert_eq!(split_keys("Esc"), vec!["Esc"]);
        assert_eq!(split_keys("+"), vec!["+"]);
        assert_eq!(split_keys("Ctrl++"), vec!["Ctrl++"]);
        assert_eq!(split_keys(" Win + Z "), vec!["Win", "Z"]);
    }

    #[test]
    fn progress_details_share_the_widest_width() {
        let bar = |detail: &str| PopupMenuEntry {
            progress: Some(0.5),
            detail: Some(detail.into()),
            ..Default::default()
        };
        let other = PopupMenuEntry {
            detail: Some("a much longer detail without a bar".into()),
            ..Default::default()
        };
        let no_cards = |_| false;
        assert_eq!(
            bar_detail_cells(&[bar("7%"), bar("69% · Reset 18.11."), other], no_cards),
            18.
        );
        // At least room for "100%"
        assert_eq!(bar_detail_cells(&[bar("1%")], no_cards), 4.);
        assert_eq!(bar_detail_cells(&[], no_cards), 4.);
        // The long detail of a card does not widen the other rows
        let card = bar("Queen · A Night at the Opera\n2:41 / 5:55");
        assert_eq!(bar_detail_cells(&[card, bar("7%")], |idx| idx == 0), 4.);
    }

    #[test]
    fn dimmed_colors_are_opaque_mixes() {
        let white = LinearRgba::with_components(1., 1., 1., 1.);
        let black = LinearRgba::with_components(0., 0., 0., 1.);
        assert_eq!(
            mix(white, black, 0.25),
            LinearRgba::with_components(0.75, 0.75, 0.75, 1.)
        );
        assert_eq!(mix(black, white.mul_alpha(0.), 1.).3, 1.);
    }

    #[test]
    fn only_items_with_an_id_can_be_chosen() {
        assert!(item(Some("a")).is_selectable());
        assert!(!item(None).is_selectable());
        assert!(!kind(PopupMenuEntryKind::Header).is_selectable());
        assert!(!kind(PopupMenuEntryKind::Separator).is_selectable());
    }

    #[test]
    fn arrow_keys_skip_what_cannot_be_chosen_and_wrap() {
        let entries = vec![
            kind(PopupMenuEntryKind::Header),
            item(Some("a")),
            kind(PopupMenuEntryKind::Separator),
            item(None),
            item(Some("b")),
        ];
        assert_eq!(next_selectable(&entries, None, 1), Some(1));
        assert_eq!(next_selectable(&entries, None, -1), Some(4));
        assert_eq!(next_selectable(&entries, Some(1), 1), Some(4));
        assert_eq!(next_selectable(&entries, Some(4), 1), Some(1));
        assert_eq!(next_selectable(&entries, Some(1), -1), Some(4));
        assert_eq!(
            next_selectable(&[kind(PopupMenuEntryKind::Header)], None, 1),
            None
        );
        assert_eq!(next_selectable(&[], None, 1), None);
    }

    #[test]
    fn a_click_on_an_item_chooses_it() {
        let press = WMEK::Press(MousePress::Left);
        assert_eq!(
            decide(&press, Some(Some(3)), None, Some(0), 1),
            MouseOutcome::Choose(3)
        );
        assert_eq!(
            decide(&press, Some(None), None, Some(0), 1),
            MouseOutcome::Consume
        );
        assert_eq!(
            decide(
                &WMEK::Press(MousePress::Right),
                Some(Some(3)),
                None,
                None,
                1
            ),
            MouseOutcome::Consume
        );
    }

    #[test]
    fn a_click_outside_closes_the_menu() {
        let press = WMEK::Press(MousePress::Left);
        // On the terminal: the click is used up by closing the menu
        assert_eq!(
            decide(&press, None, None, Some(0), 1),
            MouseOutcome::Dismiss {
                pass_through: false
            }
        );
        // On another status link: that link opens its own menu
        assert_eq!(
            decide(&press, None, Some(2), Some(0), 1),
            MouseOutcome::Dismiss { pass_through: true }
        );
        // On the link that opened it: toggles it closed
        assert_eq!(
            decide(&press, None, Some(0), Some(0), 1),
            MouseOutcome::Dismiss {
                pass_through: false
            }
        );
        assert_eq!(
            decide(&WMEK::Press(MousePress::Right), None, Some(2), Some(0), 1),
            MouseOutcome::Dismiss {
                pass_through: false
            }
        );
    }

    #[test]
    fn the_second_click_of_a_double_click_keeps_the_menu_open() {
        let press = WMEK::Press(MousePress::Left);
        assert_eq!(
            decide(&press, None, Some(0), Some(0), 2),
            MouseOutcome::Consume
        );
        // A double click elsewhere still closes it
        assert_eq!(
            decide(&press, None, None, Some(0), 2),
            MouseOutcome::Dismiss {
                pass_through: false
            }
        );
    }

    #[test]
    fn moving_highlights_and_wheel_scrolls() {
        assert_eq!(
            decide(&WMEK::Move, Some(Some(1)), None, None, 0),
            MouseOutcome::Hover {
                item: Some(1),
                consumed: true
            }
        );
        assert_eq!(
            decide(&WMEK::Move, None, None, None, 0),
            MouseOutcome::Hover {
                item: None,
                consumed: false
            }
        );
        assert_eq!(
            decide(&WMEK::VertWheel(1), Some(None), None, None, 0),
            MouseOutcome::Scroll(-1)
        );
        assert_eq!(
            decide(&WMEK::VertWheel(-1), Some(None), None, None, 0),
            MouseOutcome::Scroll(1)
        );
        assert_eq!(
            decide(&WMEK::VertWheel(1), None, None, None, 0),
            MouseOutcome::Consume
        );
        // The release of the click that opened the menu is not the menu's business
        assert_eq!(
            decide(&WMEK::Release(MousePress::Left), None, Some(0), Some(0), 1),
            MouseOutcome::Ignore
        );
    }

    fn anchor(x: f32) -> StatusLinkAnchor {
        StatusLinkAnchor {
            x,
            y: 0.,
            width: 80.,
            height: 30.,
            link: 0,
            at: Instant::now(),
        }
    }

    #[test]
    fn the_menu_opens_underneath_its_link_and_stays_inside_the_window() {
        assert_eq!(place(Some(&anchor(100.)), 300., 1000., 30.), (100., 32.));
        // Near the right edge it is shifted left
        assert_eq!(
            place(Some(&anchor(900.)), 300., 1000., 30.),
            (1000. - 300. - EDGE_GAP, 32.)
        );
        // Wider than the window: starts at the left edge
        assert_eq!(place(Some(&anchor(900.)), 2000., 1000., 30.).0, 0.);
        // Without a link: at the right, underneath the tab bar
        assert_eq!(
            place(None, 300., 1000., 30.),
            (1000. - 300. - EDGE_GAP, 32.)
        );
    }

    #[test]
    fn an_anchor_is_only_used_shortly_after_the_click() {
        let a = anchor(0.);
        assert!(a.is_fresh(a.at + Duration::from_secs(1)));
        assert!(!a.is_fresh(a.at + Duration::from_secs(6)));
    }

    #[test]
    fn scrolling_keeps_the_highlighted_item_visible() {
        let heights = vec![10.; 10];
        assert_eq!(fit_count(&heights, 0, 35.), 3);
        assert_eq!(fit_count(&heights, 8, 35.), 2);
        // Always at least one entry, even when nothing fits
        assert_eq!(fit_count(&heights, 0, 5.), 1);
        assert_eq!(scroll_to_show(&heights, 0, Some(5), 35.), 3);
        assert_eq!(scroll_to_show(&heights, 6, Some(2), 35.), 2);
        // Scrolled past the end: pulled back so that the space is used
        assert_eq!(scroll_to_show(&heights, 9, None, 35.), 7);
        assert_eq!(scroll_to_show(&heights, 0, None, 1000.), 0);
    }

    #[test]
    fn image_reads_are_limited() {
        assert_eq!(
            read_limited(&[1u8, 2, 3][..], 3).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(read_limited(&[1u8, 2, 3, 4][..], 3).unwrap(), None);
        assert_eq!(read_limited(&[][..], 3).unwrap(), Some(vec![]));
        assert!(dimensions_ok(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION));
        assert!(dimensions_ok(640, 640));
        assert!(!dimensions_ok(MAX_IMAGE_DIMENSION + 1, 10));
        assert!(!dimensions_ok(10, 100_000));
    }

    #[test]
    fn missing_or_bogus_images_do_not_panic() {
        assert!(load_image("/nonexistent/definitely/missing.png").is_none());
        let path = std::env::temp_dir().join("wezterm-popupmenu-bogus.png");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(load_image(path.to_str().unwrap()).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
