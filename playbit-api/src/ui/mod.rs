//! Archipelago's portable guest UI vocabulary.
//!
//! Guest programs encode semantic controls into a view stream. Platform hosts
//! render those controls natively and apply the interaction contracts defined
//! by this crate. Colors are semantic so one view can replace any subset of
//! the default theme without changing control code.

extern crate alloc;

pub use crate::abi::text::{TextEdit, TextEdits};
use crate::{
    abi::syscall::{Error, MAX_QUEUED_EVENTS},
    context::{Context, EventBuffer},
};

mod elements;
use alloc::vec::Vec;
pub use elements::{
    AutocompleteField, Button, Canvas, CanvasEvent, Checkbox, ContextMenu, PopupButton,
    SegmentedControl, TextEvent, TextField,
};

pub const VIEW_MAGIC: &[u8; 4] = b"PBV1";
pub const VIEW_COLUMN_START: u8 = 1;
pub const VIEW_TEXT: u8 = 2;
pub const VIEW_BUTTON: u8 = 3;
pub const VIEW_COLUMN_END: u8 = 4;
pub const VIEW_TEXT_FIELD: u8 = 5;
pub const VIEW_ROW_START: u8 = 6;
pub const VIEW_ROW_END: u8 = 7;
pub const VIEW_CHECKBOX: u8 = 8;
pub const VIEW_RADIO: u8 = 9;
pub const VIEW_THEME: u8 = 10;
pub const VIEW_POPUP_BUTTON: u8 = 11;
pub const VIEW_LIST_START: u8 = 12;
pub const VIEW_LIST_END: u8 = 13;
pub const VIEW_LIST_ROW: u8 = 14;
pub const VIEW_AUTOCOMPLETE_FIELD: u8 = 15;
pub const VIEW_CONTEXT_MENU_START: u8 = 16;
pub const VIEW_CONTEXT_MENU_END: u8 = 17;
/// Object-file image. Codes 18/19 are not accepted by the v1 syscall host.
pub const VIEW_FILE_IMAGE: u8 = 20;
pub const VIEW_IMAGE_DESCRIPTION: u8 = 21;
pub const VIEW_CANVAS: u8 = 22;
pub const VIEW_SEGMENTED_CONTROL: u8 = 23;
pub const VIEW_SCROLL_START: u8 = 24;
pub const VIEW_SCROLL_END: u8 = 25;
pub const VIEW_JPEG_IMAGE: u8 = 26;
pub const VIEW_WINDOW_STYLE: u8 = 27;
pub const VIEW_WINDOW_SIZE_CONSTRAINTS: u8 = 28;
pub const MAX_IMAGE_BYTES: usize = 128 * 1024;
pub const MAX_IMAGE_PIXELS: u32 = 1024 * 1024;

/// Bounded logical display dimensions for an image.
pub fn image_size_valid(width: u32, height: u32) -> bool {
    width != 0 && height != 0 && width <= 4096 && height <= 4096
}

pub const MIN_SEGMENTS: usize = 3;
pub const MAX_SEGMENTS: usize = 5;

/// One choice in a single-selection group. Click targets the segment ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment<'a> {
    pub id: u32,
    pub label: &'a str,
    pub disabled: bool,
}

/// Shared encoder/decoder admission for a bounded single-selection group.
pub fn segment_ids_valid(group: u32, selected: u32, ids: &[u32]) -> bool {
    if group == 0 || !(MIN_SEGMENTS..=MAX_SEGMENTS).contains(&ids.len()) {
        return false;
    }
    let mut found = false;
    for (index, &id) in ids.iter().enumerate() {
        if id == 0 || id == group || ids[..index].contains(&id) {
            return false;
        }
        found |= id == selected;
    }
    found
}
pub const MAX_CANVAS_EDGE: u32 = 4096;
/// Logical geometry only. Binary32 represents every integer up to and including 2^24.
pub const MAX_WINDOW_CANVAS_EDGE: u32 = 1 << 24;
pub const MAX_CANVAS_ITEMS: usize = 4096;
pub const MAX_CANVAS_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_CANVAS_FONT_SIZE: u32 = 96;
/// Aggregate packed RGBA8 storage admitted across every canvas in one scene.
pub const MAX_CANVAS_PIXEL_BYTES: usize = 32 * 1024 * 1024;
pub const CANVAS_PIXELS_FEATURE: &str = "ui.canvas.pixels.v1";
pub const CANVAS_VIEWPORT_FEATURE: &str = "ui.canvas.viewport.v1";
pub const CANVAS_WINDOW_FEATURE: &str = "ui.canvas.window.v1";
pub const CANVAS_CLEAR_FEATURE: &str = "ui.canvas.clear.v1";
pub const WINDOW_STYLE_FEATURE: &str = "ui.window.style.v1";
pub const WINDOW_SIZE_CONSTRAINTS_FEATURE: &str = "ui.window.size-constraints.v1";
pub const CANVAS_FLAG_CONTINUOUS: u32 = 1;
pub const CANVAS_FLAG_VIEWPORT: u32 = 2;
pub const CANVAS_FLAG_WINDOW: u32 = 4;
pub const CANVAS_FLAG_CLEAR: u32 = 8;

/// The window decorations and system appearance requested for one scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowStyle(u32);

impl WindowStyle {
    pub const TITLEBAR: Self = Self(1 << 0);
    pub const CLOSABLE: Self = Self(1 << 1);
    pub const MINIMIZABLE: Self = Self(1 << 2);
    pub const RESIZABLE: Self = Self(1 << 3);
    /// Implies full-size content.
    pub const TRANSPARENT_TITLEBAR: Self = Self(1 << 4);
    pub const HIDDEN_TITLE: Self = Self(1 << 5);
    pub const FULLSIZE_CONTENT: Self = Self(1 << 6);
    pub const THEME_BRIGHT: Self = Self(1 << 7);
    pub const THEME_DARK: Self = Self(1 << 8);
    /// Hide the host's Copy Session Link button in this window.
    pub const HIDE_SESSION_BUTTON: Self = Self(1 << 9);

    /// A titled, closable, minimizable and resizable window.
    pub const STANDARD: Self =
        Self(Self::TITLEBAR.0 | Self::CLOSABLE.0 | Self::MINIMIZABLE.0 | Self::RESIZABLE.0);
    pub const NONE: Self = Self(0);

    pub const fn from_bits(bits: u32) -> Option<Self> {
        let appearances = Self::THEME_BRIGHT.0 | Self::THEME_DARK.0;
        if bits >= (Self::HIDE_SESSION_BUTTON.0 << 1) || bits & appearances == appearances {
            return None;
        }
        Some(Self(bits))
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }
}

impl core::ops::BitOr for WindowStyle {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Window content bounds in device-independent points (dp), before layout padding.
/// A zero bound leaves that edge unrestricted. A zero width/height ratio releases the lock.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowSizeConstraints {
    pub min_width: f32,
    pub min_height: f32,
    pub max_width: f32,
    pub max_height: f32,
    pub aspect_ratio: f32,
}

impl WindowSizeConstraints {
    /// Whether the bounds admit a positive content size with the requested aspect ratio.
    pub fn is_valid(self) -> bool {
        let edge_limit = MAX_WINDOW_CANVAS_EDGE as f32;
        let width_max = if self.max_width == 0.0 { edge_limit } else { self.max_width };
        let height_max = if self.max_height == 0.0 { edge_limit } else { self.max_height };
        let ratio = self.aspect_ratio;
        (0.0..=width_max).contains(&self.min_width)
            && width_max <= edge_limit
            && (0.0..=height_max).contains(&self.min_height)
            && height_max <= edge_limit
            && (ratio == 0.0
                || ratio.is_finite()
                    && ratio > 0.0
                    && self.min_width / height_max <= ratio
                    && (self.min_height == 0.0 || ratio <= width_max / self.min_height))
    }

    fn words(self) -> [u32; 5] {
        [self.min_width, self.min_height, self.max_width, self.max_height, self.aspect_ratio]
            .map(|value| if value == 0.0 { 0 } else { value.to_bits() })
    }
}

// Compare the canonical scene words so equality is total even for an unsubmitted NaN.
impl PartialEq for WindowSizeConstraints {
    fn eq(&self, other: &Self) -> bool {
        self.words() == other.words()
    }
}
impl Eq for WindowSizeConstraints {}

/// Frame events are opt-in. Static canvases redraw on scene changes without
/// keeping a display callback alive while there is no app work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum CanvasFrames {
    Static = 0,
    Continuous = CANVAS_FLAG_CONTINUOUS,
}

/// Inline canvases participate in layout. Viewport and Window canvases are
/// clipped scene roots and receive their available size through SurfaceChanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum CanvasExtent {
    #[default]
    Inline = 0,
    /// The padded content area; requires [CANVAS_VIEWPORT_FEATURE].
    Viewport = CANVAS_FLAG_VIEWPORT,
    /// The whole drawable window, including beneath the titlebar. Requires [CANVAS_WINDOW_FEATURE].
    Window = CANVAS_FLAG_WINDOW,
}

impl CanvasExtent {
    pub const fn max_edge(self) -> u32 {
        match self {
            Self::Inline | Self::Viewport => MAX_CANVAS_EDGE,
            Self::Window => MAX_WINDOW_CANVAS_EDGE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanvasOptions {
    pub frames: CanvasFrames,
    pub extent: CanvasExtent,
    /// Paint beneath every item. None leaves the canvas transparent.
    /// A specified color requires [CANVAS_CLEAR_FEATURE].
    pub clear_color: Option<Color>,
}

impl Default for CanvasOptions {
    fn default() -> Self {
        Self { frames: CanvasFrames::Static, extent: CanvasExtent::Inline, clear_color: None }
    }
}

/// A filled rectangle in canvas coordinates; color is Display P3 0xRRGGBBAA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanvasRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub color: Color,
}

/// A clipped single-line label using the host UI font. The SDK borrows its text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanvasText<'a> {
    pub bounds: CanvasRect,
    pub font_size: u32,
    pub bold: bool,
    pub text: &'a str,
}

/// Tightly packed, straight-alpha sRGB RGBA8 pixels, sampled nearest-neighbor.
/// `bounds.color` is reserved and must be zero. Destination bounds may scale the
/// source independently in either axis; source pixels are never interpolated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanvasPixels<'a> {
    pub bounds: CanvasRect,
    pub source_width: u32,
    pub source_height: u32,
    pub rgba: &'a [u8],
}

impl CanvasPixels<'_> {
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.bounds.fits(width, height)
            && self.bounds.color.0 == 0
            && (1..=MAX_CANVAS_EDGE).contains(&self.source_width)
            && (1..=MAX_CANVAS_EDGE).contains(&self.source_height)
            && self.rgba.len() <= MAX_CANVAS_PIXEL_BYTES
            && u64::from(self.source_width) * u64::from(self.source_height) * 4
                == self.rgba.len() as u64
    }
}

impl CanvasText<'_> {
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.bounds.fits(width, height)
            && (1..=MAX_CANVAS_FONT_SIZE).contains(&self.font_size)
            && self.text.len() <= crate::abi::syscall::MAX_STRING_BYTES
            && !self.text.chars().any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanvasItem<'a> {
    Rect(CanvasRect),
    Text(CanvasText<'a>),
    Pixels(CanvasPixels<'a>),
}

impl CanvasRect {
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.width != 0
            && self.height != 0
            && self.x.checked_add(self.width).is_some_and(|end| end <= width)
            && self.y.checked_add(self.height).is_some_and(|end| end <= height)
    }
}

/// An independent blob reached through this guest's scoped volume handle.
/// This is neither a host path nor a transferable authority token.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FileReference {
    pub volume: crate::abi::syscall::VolumeHandle,
    pub entry_id: [u8; 15],
}

pub const CONTROL_DISABLED: u8 = 1;
pub const TEXT_FIELD_MULTILINE: u8 = 2;
pub const TEXT_FIELD_READ_ONLY: u8 = 4;
pub const MENU_CHECKED: u8 = 2;
pub const MENU_CHECKABLE: u8 = 8;
pub const MENU_DESTRUCTIVE: u8 = 4;
pub const MENU_ACTION: u8 = 1;
pub const MENU_SEPARATOR: u8 = 2;
pub const MENU_SUBMENU: u8 = 3;
pub const LIST_SELECTED: u8 = 2;
pub const LIST_INACTIVE: u8 = 4;
pub const LIST_HAS_CHILDREN: u8 = 8;
const MAX_CONTAINER_DEPTH: usize = 32;
const MAX_MENU_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TextStyle {
    Body,
    Title,
    Muted,
    Label,
    SectionTitle,
    LabelLarge,
}

impl TextStyle {
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Body),
            1 => Some(Self::Title),
            2 => Some(Self::Muted),
            3 => Some(Self::Label),
            4 => Some(Self::SectionTitle),
            5 => Some(Self::LabelLarge),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ButtonStyle {
    Default,
    Primary,
    Destructive,
}

impl ButtonStyle {
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Default),
            1 => Some(Self::Primary),
            2 => Some(Self::Destructive),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CheckState {
    Off,
    On,
    Mixed,
}

impl CheckState {
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            2 => Some(Self::Mixed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ThemeColor {
    WindowBackground,
    PanelBackground,
    ControlBackground,
    ControlPressed,
    ControlDisabled,
    Text,
    TextSecondary,
    TextDisabled,
    Border,
    FocusRing,
    TextSelection,
    TextSelectionText,
    PrimaryBackground,
    PrimaryPressed,
    PrimaryText,
    DestructiveBackground,
    DestructivePressed,
    DestructiveText,
    MenuBackground,
    MenuBorder,
    MenuSelection,
    MenuSelectionText,
    ColumnSelectionInactive,
    ColumnSelectionInactiveText,
    Separator,
    Shadow,
}

impl ThemeColor {
    pub const COUNT: usize = 26;

    pub fn from_wire(value: u8) -> Option<Self> {
        const COLORS: [ThemeColor; ThemeColor::COUNT] = [
            ThemeColor::WindowBackground,
            ThemeColor::PanelBackground,
            ThemeColor::ControlBackground,
            ThemeColor::ControlPressed,
            ThemeColor::ControlDisabled,
            ThemeColor::Text,
            ThemeColor::TextSecondary,
            ThemeColor::TextDisabled,
            ThemeColor::Border,
            ThemeColor::FocusRing,
            ThemeColor::TextSelection,
            ThemeColor::TextSelectionText,
            ThemeColor::PrimaryBackground,
            ThemeColor::PrimaryPressed,
            ThemeColor::PrimaryText,
            ThemeColor::DestructiveBackground,
            ThemeColor::DestructivePressed,
            ThemeColor::DestructiveText,
            ThemeColor::MenuBackground,
            ThemeColor::MenuBorder,
            ThemeColor::MenuSelection,
            ThemeColor::MenuSelectionText,
            ThemeColor::ColumnSelectionInactive,
            ThemeColor::ColumnSelectionInactiveText,
            ThemeColor::Separator,
            ThemeColor::Shadow,
        ];
        COLORS.get(usize::from(value)).copied()
    }
}

/// A Display P3 color encoded as `0xRRGGBBAA`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color(pub u32);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Theme {
    colors: [Color; ThemeColor::COUNT],
}

impl Theme {
    pub const fn light() -> Self {
        Self {
            colors: [
                Color(0xecececff),
                Color(0xecececff),
                Color(0xffffffff),
                Color(0x00000026),
                Color(0x0000000d),
                Color(0x000000e6),
                Color(0x00000066),
                Color(0x0000004d),
                Color(0x0000004d),
                Color(0x000000e6),
                Color(0xc2d8ffff),
                Color(0x000000cc),
                Color(0x000000c0),
                Color(0x00000099),
                Color(0xffffffff),
                Color(0xd7290eff),
                Color(0xa91700ff),
                Color(0xffffffff),
                Color(0xf1f1f1ff),
                Color(0x0000004d),
                Color(0x6f6f6fff),
                Color(0xffffffff),
                Color(0x0000001a),
                Color(0x000000cc),
                Color(0x00000033),
                Color(0x00000026),
            ],
        }
    }

    pub fn color(&self, role: ThemeColor) -> Color {
        self.colors[role as usize]
    }

    pub fn set_color(&mut self, role: ThemeColor, color: Color) {
        self.colors[role as usize] = color;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeOverride {
    pub role: ThemeColor,
    pub color: Color,
}

/// Built-in monochrome menu symbols, rendered at 16dp in the row's foreground color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MenuIcon {
    Avatar = 1,
    Signature = 2,
    Copy = 3,
    ExitRight = 4,
    OpenBook = 5,
    Talk = 6,
}

impl MenuIcon {
    pub fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Avatar),
            2 => Some(Self::Signature),
            3 => Some(Self::Copy),
            4 => Some(Self::ExitRight),
            5 => Some(Self::OpenBook),
            6 => Some(Self::Talk),
            _ => None,
        }
    }
}

/// A checkable action uses `Some(false)` or `Some(true)`; ordinary actions use `None`.
/// Icon columns align within each section, checkmark columns across the entire menu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuItem<'a> {
    Action {
        id: u32,
        label: &'a str,
        icon: Option<MenuIcon>,
        checked: Option<bool>,
        disabled: bool,
        destructive: bool,
    },
    Separator,
    Submenu {
        label: &'a str,
        icon: Option<MenuIcon>,
        items: &'a [MenuItem<'a>],
        disabled: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutocompleteItem<'a> {
    pub label: &'a str,
    pub value: &'a str,
}

/// Encoder borrowing SDK-owned scene storage and an immutable turn snapshot.
/// Invalid elements, nesting, and lengths that cannot fit the wire format panic.
pub struct ViewEncoder<'a> {
    bytes: &'a mut Vec<u8>,
    events: &'a EventBuffer,
    applied_checkbox_clicks: [bool; MAX_QUEUED_EVENTS],
    stack: [u8; MAX_CONTAINER_DEPTH],
    child_counts: [u32; MAX_CONTAINER_DEPTH],
    depth: usize,
    roots: u32,
    theme_written: bool,
    window_style_written: bool,
    window_size_constraints_written: bool,
    canvas_items: usize,
    canvas_text_bytes: usize,
    canvas_pixel_bytes: usize,
}

impl<'a> ViewEncoder<'a> {
    pub(crate) fn new(bytes: &'a mut Vec<u8>, events: &'a EventBuffer) -> Self {
        bytes.clear();
        bytes.extend_from_slice(VIEW_MAGIC);
        Self {
            bytes,
            events,
            applied_checkbox_clicks: [false; MAX_QUEUED_EVENTS],
            stack: [0; MAX_CONTAINER_DEPTH],
            child_counts: [0; MAX_CONTAINER_DEPTH],
            depth: 0,
            roots: 0,
            theme_written: false,
            window_style_written: false,
            window_size_constraints_written: false,
            canvas_items: 0,
            canvas_text_bytes: 0,
            canvas_pixel_bytes: 0,
        }
    }

    /// Sets partial semantic overrides for this view. Missing roles inherit
    /// from the host's light theme. Call this before starting the root.
    pub fn theme(&mut self, overrides: &[ThemeOverride]) {
        assert_eq!(self.depth, 0, "theme must precede the root");
        assert_eq!(self.roots, 0, "theme must precede the root");
        assert!(!self.theme_written, "theme may only be written once");

        let payload_len = overrides.len().checked_mul(5).expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.write_header(VIEW_THEME, 0, payload_len);
        for item in overrides {
            self.write_byte(item.role as u8);
            self.write(&item.color.0.to_le_bytes());
        }
        self.theme_written = true;
    }

    /// Set the native window style for this scene before starting its root.
    /// Omitting the style restores host defaults. Requires [WINDOW_STYLE_FEATURE].
    pub fn window_style(&mut self, style: WindowStyle) {
        assert_eq!(self.depth, 0, "window style must precede the root");
        assert_eq!(self.roots, 0, "window style must precede the root");
        assert!(!self.window_style_written, "window style may only be written once");
        assert!(WindowStyle::from_bits(style.bits()).is_some(), "conflicting window appearances");
        self.command(VIEW_WINDOW_STYLE, 0, &style.bits().to_le_bytes());
        self.window_style_written = true;
    }

    /// Set the content size limits and width/height ratio before starting this scene's root.
    /// Omission releases all guest size constraints. Requires [WINDOW_SIZE_CONSTRAINTS_FEATURE].
    pub fn window_size_constraints(&mut self, constraints: WindowSizeConstraints) {
        assert!(
            self.depth == 0 && self.roots == 0,
            "window size constraints must precede the root"
        );
        assert!(
            !self.window_size_constraints_written,
            "window size constraints may only be written once"
        );
        assert!(constraints.is_valid(), "incompatible window size constraints");
        let words = constraints.words().map(u32::to_le_bytes);
        self.command(VIEW_WINDOW_SIZE_CONSTRAINTS, 0, words.as_flattened());
        self.window_size_constraints_written = true;
    }

    pub fn column_start(&mut self) {
        self.container_start(VIEW_COLUMN_START)
    }

    pub fn column_end(&mut self) {
        self.container_end(VIEW_COLUMN_START, VIEW_COLUMN_END)
    }

    /// Begin a viewport containing one child, sized by its parent layout.
    /// Scroll offsets belong to the host,
    /// so scrolling never requires a guest turn or network round trip.
    /// `complex_background` requests contrast over images or changing artwork.
    pub fn scroll_start(&mut self, id: u32, complex_background: bool) {
        assert_ne!(id, 0, "element ID must be nonzero");
        assert!(self.depth < self.stack.len(), "view exceeds maximum container depth");

        self.reserve_command(1);
        self.begin_node();
        self.write_header(VIEW_SCROLL_START, id, 1);
        self.write_byte(u8::from(complex_background));
        self.stack[self.depth] = VIEW_SCROLL_START;
        self.child_counts[self.depth] = 0;
        self.depth += 1;
    }

    pub fn scroll_end(&mut self) {
        self.container_end(VIEW_SCROLL_START, VIEW_SCROLL_END)
    }

    /// Embedded sRGB JPEG artwork (up to 128 KiB and 1 megapixel decoded).
    /// Width/height are logical display dimensions; decoding errors are visible
    /// presentation errors. Use object-file images for data stored in a volume.
    pub fn jpeg_image(&mut self, id: u32, width: u32, height: u32, jpeg: &[u8], description: &str) {
        self.require_element(id);
        assert!(image_size_valid(width, height), "invalid image dimensions");
        assert!(!jpeg.is_empty(), "JPEG image must not be empty");
        assert!(jpeg.len() <= MAX_IMAGE_BYTES, "JPEG image exceeds byte limit");
        assert!(description.len() <= 4096, "image description exceeds byte limit");

        let payload = 12 + description.len() + jpeg.len();
        self.reserve_command(payload);
        self.begin_node();
        self.write_header(VIEW_JPEG_IMAGE, id, payload);
        self.write_u32(width);
        self.write_u32(height);
        self.write_u32(description.len() as u32);
        self.write(description.as_bytes());
        self.write(jpeg)
    }

    pub fn row_start(&mut self) {
        self.container_start(VIEW_ROW_START)
    }

    pub fn row_end(&mut self) {
        self.container_end(VIEW_ROW_START, VIEW_ROW_END)
    }

    /// Begins a region whose single child opens `items` on secondary click.
    pub fn context_menu_start<'b>(&'b mut self, items: &'b [MenuItem<'b>]) -> ContextMenu<'b, 'a> {
        assert!(!items.is_empty(), "menu must contain at least one item");
        assert!(self.depth < self.stack.len(), "view exceeds maximum container depth");

        let entries_len = menu_entries_len(items, 0);
        let payload_len = 4usize.checked_add(entries_len).expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_CONTEXT_MENU_START, 0, payload_len);
        self.write_u32(u32::try_from(items.len()).expect("view length must fit u32"));
        self.write_menu_entries(items, 0);
        self.stack[self.depth] = VIEW_CONTEXT_MENU_START;
        self.child_counts[self.depth] = 0;
        self.depth += 1;
        ContextMenu { encoder: self, items }
    }

    pub fn context_menu_end(&mut self) {
        self.container_end(VIEW_CONTEXT_MENU_START, VIEW_CONTEXT_MENU_END)
    }

    pub fn text(&mut self, style: TextStyle, text: &str) {
        self.require_parent();

        let payload_len = text.len().checked_add(1).expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_TEXT, 0, payload_len);
        self.write_byte(style as u8);
        self.write(text.as_bytes())
    }

    pub fn button(
        &mut self,
        id: u32,
        label: &str,
        style: ButtonStyle,
        disabled: bool,
    ) -> Button<'_, 'a> {
        self.require_element(id);

        let payload_len = label.len().checked_add(2).expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_BUTTON, id, payload_len);
        self.write_byte(style as u8);
        self.write_byte(if disabled { CONTROL_DISABLED } else { 0 });
        self.write(label.as_bytes());
        Button { encoder: self, id }
    }

    /// Display an object file in a point-sized box. This node is encoded and
    /// decoded today, but no shipping host renders `ViewNode::FileImage` yet;
    /// a host that does render it must recheck current object access and
    /// validate the authenticated content before drawing it. Describe the
    /// image in at most 4,096 UTF-8 bytes; an empty description marks a
    /// decorative image that native accessibility should omit.
    pub fn file_image(&mut self, file: FileReference, width: u32, height: u32, alt_text: &str) {
        assert!(
            alt_text.len() <= crate::abi::syscall::MAX_STRING_BYTES,
            "image description exceeds byte limit"
        );
        assert_ne!(file.volume.generation, 0, "file volume generation must be nonzero");
        assert_ne!(file.volume.slot, 0, "file volume slot must be nonzero");
        assert_ne!(file.entry_id, [0; 15], "file entry ID must be nonzero");
        assert!(image_size_valid(width, height), "invalid image dimensions");
        self.require_parent();

        // Reserve both image commands together to avoid growing twice.
        self.reserve_command(31 + 9 + alt_text.len());
        self.begin_node();
        self.write_header(VIEW_FILE_IMAGE, 0, 31);
        self.write_u32(file.volume.slot);
        self.write_u32(file.volume.generation);
        self.write(&file.entry_id);
        self.write_u32(width);
        self.write_u32(height);
        self.command(VIEW_IMAGE_DESCRIPTION, 0, alt_text.as_bytes())
    }

    /// Draw a transparent inline canvas with a nonzero input target. Frame input is opt-in.
    pub fn canvas(
        &mut self,
        id: u32,
        width: u32,
        height: u32,
        frames: CanvasFrames,
        items: &[CanvasItem<'_>],
    ) -> Canvas<'_, 'a> {
        assert_ne!(id, 0, "element ID must be nonzero");
        self.canvas_with_options(
            id,
            width,
            height,
            CanvasOptions { frames, ..Default::default() },
            items,
        )
    }

    /// Draw ordered items with the given frame mode, extent and optional clear color.
    /// Limits count all canvases in this scene, not each control independently.
    /// Payload: u32 width/height/flags/count, optional clear color, then tagged items. Each item
    /// has u32 kind (0 rectangle, 1 text, 2 pixels), x/y/width/height/color. Text adds u32
    /// font size, bold (0/1), UTF-8 byte length, then exactly those bytes.
    /// Pixels require `ui.canvas.pixels.v1` and add u32 source width/height and
    /// byte length, then exactly source width * source height * 4 RGBA8 bytes.
    pub fn canvas_with_options(
        &mut self,
        id: u32,
        width: u32,
        height: u32,
        options: CanvasOptions,
        items: &[CanvasItem<'_>],
    ) -> Canvas<'_, 'a> {
        if options.extent == CanvasExtent::Inline {
            self.require_parent();
        } else {
            assert!(self.stack[..self.depth].iter().all(|kind| *kind == VIEW_CONTEXT_MENU_START));
        }
        assert!(id != 0 || options.frames == CanvasFrames::Static);
        self.encode_canvas(id, width, height, options, items.iter().copied());
        Canvas { encoder: self, id }
    }

    /// Draw a transparent root canvas in the padded content viewport.
    /// Requires [CANVAS_VIEWPORT_FEATURE]. A context menu may wrap the canvas.
    pub fn viewport_canvas(
        &mut self,
        id: u32,
        width: u32,
        height: u32,
        frames: CanvasFrames,
        items: &[CanvasItem<'_>],
    ) -> Canvas<'_, 'a> {
        let options = CanvasOptions { frames, extent: CanvasExtent::Viewport, clear_color: None };
        self.canvas_with_options(id, width, height, options, items)
    }

    /// Draw rectangles without pointer, key, or animation-frame input.
    /// The reserved zero ID marks this canvas as decorative and noninteractive.
    pub fn canvas_static(&mut self, width: u32, height: u32, rects: &[CanvasRect]) {
        self.require_parent();
        self.encode_canvas(
            0,
            width,
            height,
            CanvasOptions::default(),
            rects.iter().copied().map(CanvasItem::Rect),
        )
    }

    fn encode_canvas<'b, I>(
        &mut self,
        id: u32,
        width: u32,
        height: u32,
        options: CanvasOptions,
        items: I,
    ) where
        I: ExactSizeIterator<Item = CanvasItem<'b>> + Clone,
    {
        let edges = 1..=options.extent.max_edge();
        assert!(edges.contains(&width), "invalid canvas width");
        assert!(edges.contains(&height), "invalid canvas height");
        assert!(
            items.len() <= MAX_CANVAS_ITEMS - self.canvas_items,
            "scene exceeds canvas item limit"
        );

        let mut text_bytes = self.canvas_text_bytes;
        let mut pixel_bytes = self.canvas_pixel_bytes;
        let item_count = items.len();
        let mut payload_len = 16 + 4 * usize::from(options.clear_color.is_some()) + item_count * 24;
        for item in items.clone() {
            match item {
                CanvasItem::Rect(rect) if rect.fits(width, height) => {}
                CanvasItem::Text(text) if text.fits(width, height) => {
                    assert!(
                        text.text.len() <= MAX_CANVAS_TEXT_BYTES - text_bytes,
                        "scene exceeds canvas text limit"
                    );
                    text_bytes += text.text.len();
                    payload_len += 12 + text.text.len();
                }
                CanvasItem::Pixels(pixels) if pixels.fits(width, height) => {
                    assert!(
                        pixels.rgba.len() <= MAX_CANVAS_PIXEL_BYTES - pixel_bytes,
                        "scene exceeds canvas pixel limit"
                    );
                    pixel_bytes += pixels.rgba.len();
                    payload_len += 12 + pixels.rgba.len();
                }
                _ => panic!("invalid canvas item"),
            }
        }
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_CANVAS, id, payload_len);
        self.write_u32(width);
        self.write_u32(height);
        let clear = if options.clear_color.is_some() { CANVAS_FLAG_CLEAR } else { 0 };
        self.write_u32(options.frames as u32 | options.extent as u32 | clear);
        self.write_u32(item_count as u32);
        if let Some(color) = options.clear_color {
            self.write_u32(color.0);
        }
        for item in items {
            self.encode_canvas_item(item);
        }
        self.canvas_items += item_count;
        self.canvas_text_bytes = text_bytes;
        self.canvas_pixel_bytes = pixel_bytes;
    }

    fn encode_canvas_item(&mut self, item: CanvasItem<'_>) {
        let (kind, rect) = match item {
            CanvasItem::Rect(rect) => (0, rect),
            CanvasItem::Text(text) => (1, text.bounds),
            CanvasItem::Pixels(pixels) => (2, pixels.bounds),
        };
        self.write_u32(kind);
        for value in [rect.x, rect.y, rect.width, rect.height, rect.color.0] {
            self.write_u32(value);
        }
        match item {
            CanvasItem::Text(text) => {
                self.write_u32(text.font_size);
                self.write_u32(u32::from(text.bold));
                self.write_u32(text.text.len() as u32);
                self.write(text.text.as_bytes());
            }
            CanvasItem::Pixels(pixels) => {
                self.write_u32(pixels.source_width);
                self.write_u32(pixels.source_height);
                self.write_u32(pixels.rgba.len() as u32);
                self.write(pixels.rgba);
            }
            CanvasItem::Rect(_) => {}
        }
    }

    /// Applies the latest delivered edit or submission before encoding the field.
    /// The response retains every event, even when the field is now disabled or read-only.
    pub fn text_field(
        &mut self,
        id: u32,
        value: &mut String,
        placeholder: &str,
        multiline: bool,
        disabled: bool,
        read_only: bool,
    ) -> TextField<'_, 'a> {
        self.require_element(id);
        if let Some(TextEvent::Changed { text, .. } | TextEvent::Submitted(text)) =
            elements::text_events(self.events, id).next_back()
        {
            text.clone_into(value);
        }

        let payload_len = 5usize
            .checked_add(value.len())
            .and_then(|length| length.checked_add(placeholder.len()))
            .expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_TEXT_FIELD, id, payload_len);
        let mut flags = 0;
        if disabled {
            flags |= CONTROL_DISABLED;
        }
        if multiline {
            flags |= TEXT_FIELD_MULTILINE;
        }
        if read_only {
            flags |= TEXT_FIELD_READ_ONLY;
        }
        self.write_byte(flags);
        self.write_u32(u32::try_from(value.len()).expect("view length must fit u32"));
        self.write(value.as_bytes());
        self.write(placeholder.as_bytes());
        TextField { encoder: self, id }
    }

    /// Applies each delivered click once before encoding the state; mixed changes to on.
    /// Delivered clicks still apply when the checkbox is now disabled.
    /// Rebuilding with this encoder preserves the updated state without replaying clicks.
    pub fn checkbox(
        &mut self,
        id: u32,
        label: &str,
        state: &mut CheckState,
        disabled: bool,
    ) -> Checkbox<'_, 'a> {
        self.require_element(id);
        for index in self.events.click_indices(id) {
            if !self.applied_checkbox_clicks[index] {
                *state = match *state {
                    CheckState::Off | CheckState::Mixed => CheckState::On,
                    CheckState::On => CheckState::Off,
                };
                self.applied_checkbox_clicks[index] = true;
            }
        }

        let payload_len = label.len().checked_add(2).expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_CHECKBOX, id, payload_len);
        self.write_byte(*state as u8);
        self.write_byte(if disabled { CONTROL_DISABLED } else { 0 });
        self.write(label.as_bytes());
        Checkbox { encoder: self, id }
    }

    /// Exactly one selected segment; use a checkbox for two choices and a
    /// pop-up for more than five. Disabled selection may be set by app state.
    pub fn segmented_control<'b>(
        &'b mut self,
        id: u32,
        segments: &'b [Segment<'b>],
        selected: u32,
        disabled: bool,
    ) -> SegmentedControl<'b, 'a> {
        self.require_element(id);
        assert!((MIN_SEGMENTS..=MAX_SEGMENTS).contains(&segments.len()), "invalid segment count");

        let mut ids = [0; MAX_SEGMENTS];
        let mut payload_len = 6usize;
        for (index, segment) in segments.iter().enumerate() {
            ids[index] = segment.id;
            payload_len = payload_len
                .checked_add(9)
                .and_then(|n| n.checked_add(segment.label.len()))
                .expect("view payload length overflow");
        }
        assert!(
            segment_ids_valid(id, selected, &ids[..segments.len()]),
            "invalid segment IDs or selection"
        );

        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_SEGMENTED_CONTROL, id, payload_len);
        self.write_byte(if disabled { CONTROL_DISABLED } else { 0 });
        self.write_u32(selected);
        self.write_byte(segments.len() as u8);
        for segment in segments {
            self.write_u32(segment.id);
            self.write_byte(if segment.disabled { CONTROL_DISABLED } else { 0 });
            self.write_u32(u32::try_from(segment.label.len()).expect("view length must fit u32"));
            self.write(segment.label.as_bytes());
        }
        SegmentedControl { encoder: self, segments }
    }

    pub fn popup_button<'b>(
        &'b mut self,
        id: u32,
        label: &str,
        items: &'b [MenuItem<'b>],
        disabled: bool,
    ) -> PopupButton<'b, 'a> {
        self.require_element(id);
        assert!(!items.is_empty(), "menu must contain at least one item");

        let entries_len = menu_entries_len(items, 0);
        let payload_len = 9usize
            .checked_add(label.len())
            .and_then(|length| length.checked_add(entries_len))
            .expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_POPUP_BUTTON, id, payload_len);
        self.write_byte(if disabled { CONTROL_DISABLED } else { 0 });
        self.write_u32(u32::try_from(label.len()).expect("view length must fit u32"));
        self.write(label.as_bytes());
        self.write_u32(u32::try_from(items.len()).expect("view length must fit u32"));
        self.write_menu_entries(items, 0);
        PopupButton { encoder: self, items }
    }

    pub fn autocomplete_field(
        &mut self,
        id: u32,
        value: &str,
        placeholder: &str,
        suggestions: &[AutocompleteItem<'_>],
        disabled: bool,
    ) -> AutocompleteField<'_, 'a> {
        self.require_element(id);
        assert!(!suggestions.is_empty(), "autocomplete must contain at least one suggestion");

        let suggestions_len = autocomplete_entries_len(suggestions);
        let payload_len = 13usize
            .checked_add(value.len())
            .and_then(|length| length.checked_add(placeholder.len()))
            .and_then(|length| length.checked_add(suggestions_len))
            .expect("view payload length overflow");
        self.reserve_command(payload_len);
        self.begin_node();
        self.write_header(VIEW_AUTOCOMPLETE_FIELD, id, payload_len);
        self.write_byte(if disabled { CONTROL_DISABLED } else { 0 });
        self.write_u32(u32::try_from(value.len()).expect("view length must fit u32"));
        self.write_u32(u32::try_from(placeholder.len()).expect("view length must fit u32"));
        self.write_u32(u32::try_from(suggestions.len()).expect("view length must fit u32"));
        self.write(value.as_bytes());
        self.write(placeholder.as_bytes());
        for suggestion in suggestions {
            self.write_u32(
                u32::try_from(suggestion.label.len()).expect("view length must fit u32"),
            );
            self.write_u32(
                u32::try_from(suggestion.value.len()).expect("view length must fit u32"),
            );
            self.write(suggestion.label.as_bytes());
            self.write(suggestion.value.as_bytes());
        }
        AutocompleteField { encoder: self, id }
    }

    /// Encoded byte count, including the view header.
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Submits the frame and retains the scene buffer's capacity.
    /// The frame is cleared even when the host rejects the submission.
    pub fn submit(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        assert_eq!(self.depth, 0, "view contains unclosed containers");
        assert_eq!(self.roots, 1, "view must contain exactly one root");

        let result = context.submit_view(self.bytes);
        self.bytes.truncate(VIEW_MAGIC.len());
        self.roots = 0;
        self.theme_written = false;
        self.window_style_written = false;
        self.window_size_constraints_written = false;
        self.canvas_items = 0;
        self.canvas_text_bytes = 0;
        self.canvas_pixel_bytes = 0;

        result
    }

    /// Takes the encoded view without copying its buffer. Use `submit` to reuse it.
    pub fn finish(self) -> Vec<u8> {
        assert_eq!(self.depth, 0, "view contains unclosed containers");
        assert_eq!(self.roots, 1, "view must contain exactly one root");

        core::mem::take(self.bytes)
    }

    fn write_menu_entries(&mut self, items: &[MenuItem<'_>], depth: usize) {
        assert!(depth < MAX_MENU_DEPTH, "menu exceeds maximum depth");
        for item in items {
            match item {
                MenuItem::Action { id, label, icon, checked, disabled, destructive } => {
                    assert_ne!(*id, 0, "menu action ID must be nonzero");
                    self.write_byte(MENU_ACTION);
                    self.write_u32(*id);
                    let mut flags = 0;
                    if *disabled {
                        flags |= CONTROL_DISABLED;
                    }
                    if let Some(checked) = checked {
                        flags |= MENU_CHECKABLE;
                        if *checked {
                            flags |= MENU_CHECKED;
                        }
                    }
                    if *destructive {
                        flags |= MENU_DESTRUCTIVE;
                    }
                    self.write_byte(flags);
                    self.write_byte(icon.map_or(0, |icon| icon as u8));
                    self.write_u32(u32::try_from(label.len()).expect("view length must fit u32"));
                    self.write(label.as_bytes());
                }
                MenuItem::Separator => self.write_byte(MENU_SEPARATOR),
                MenuItem::Submenu { label, icon, items, disabled } => {
                    assert!(!items.is_empty(), "submenu must contain at least one item");
                    self.write_byte(MENU_SUBMENU);
                    self.write_byte(if *disabled { CONTROL_DISABLED } else { 0 });
                    self.write_byte(icon.map_or(0, |icon| icon as u8));
                    self.write_u32(u32::try_from(label.len()).expect("view length must fit u32"));
                    self.write(label.as_bytes());
                    self.write_u32(u32::try_from(items.len()).expect("view length must fit u32"));
                    self.write_menu_entries(items, depth + 1);
                }
            }
        }
    }

    fn container_start(&mut self, kind: u8) {
        assert!(self.depth < self.stack.len(), "view exceeds maximum container depth");
        self.reserve_command(0);
        self.begin_node();
        self.command(kind, 0, &[]);
        self.stack[self.depth] = kind;
        self.child_counts[self.depth] = 0;
        self.depth += 1;
    }

    fn container_end(&mut self, start: u8, end: u8) {
        assert!(self.depth > 0, "no container to close");
        assert_eq!(self.stack[self.depth - 1], start, "mismatched container end");
        if matches!(start, VIEW_CONTEXT_MENU_START | VIEW_SCROLL_START) {
            assert_eq!(
                self.child_counts[self.depth - 1],
                1,
                "container must contain exactly one child"
            );
        }
        self.command(end, 0, &[]);
        self.depth -= 1;
    }

    fn require_parent(&self) {
        assert!(self.depth > 0, "element requires a parent container");
    }

    fn require_element(&self, id: u32) {
        self.require_parent();
        assert_ne!(id, 0, "element ID must be nonzero");
    }

    fn begin_node(&mut self) {
        if self.depth == 0 {
            self.roots = self.roots.checked_add(1).expect("view node count overflow");
        } else {
            self.child_counts[self.depth - 1] =
                self.child_counts[self.depth - 1].checked_add(1).expect("view node count overflow");
        }
    }

    fn command(&mut self, kind: u8, id: u32, payload: &[u8]) {
        self.reserve_command(payload.len());
        self.write_header(kind, id, payload.len());
        self.write(payload)
    }

    fn reserve_command(&mut self, payload_len: usize) {
        assert!(u32::try_from(payload_len).is_ok(), "view payload exceeds wire length limit");
        let additional = 9usize.checked_add(payload_len).expect("view command length overflow");
        self.bytes.reserve(additional);
    }

    fn write_header(&mut self, kind: u8, id: u32, payload_len: usize) {
        self.write_byte(kind);
        self.write_u32(id);
        self.write_u32(u32::try_from(payload_len).expect("view length must fit u32"))
    }

    fn write_byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn write_u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes())
    }

    fn write(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }
}

fn menu_entries_len(items: &[MenuItem<'_>], depth: usize) -> usize {
    assert!(depth < MAX_MENU_DEPTH, "menu exceeds maximum depth");
    let mut length = 0usize;
    for item in items {
        let item_length = match item {
            MenuItem::Action { id, label, .. } => {
                assert_ne!(*id, 0, "menu action ID must be nonzero");
                11usize.checked_add(label.len()).expect("view payload length overflow")
            }
            MenuItem::Separator => 1,
            MenuItem::Submenu { label, items, .. } => {
                assert!(!items.is_empty(), "submenu must contain at least one item");
                let nested_length = menu_entries_len(items, depth + 1);
                11usize
                    .checked_add(label.len())
                    .and_then(|value| value.checked_add(nested_length))
                    .expect("view payload length overflow")
            }
        };
        length = length.checked_add(item_length).expect("view payload length overflow");
    }
    length
}

fn autocomplete_entries_len(items: &[AutocompleteItem<'_>]) -> usize {
    let mut length = 0usize;
    for item in items {
        length = length
            .checked_add(8)
            .and_then(|value| value.checked_add(item.label.len()))
            .and_then(|value| value.checked_add(item.value.len()))
            .expect("view payload length overflow");
    }
    length
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    extern crate std;

    pub(super) fn assert_panics(action: impl FnOnce()) {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).is_err());
    }

    #[test]
    fn window_size_constraints_encode_fractional_dp_and_canonical_zero() {
        // An upper bound below one dp is meaningful on a scaled display.
        let constraints = WindowSizeConstraints {
            min_width: 0.5,
            min_height: -0.0,
            max_width: 0.75,
            max_height: 0.25,
            aspect_ratio: 2.0,
        };
        let mut state = crate::context::ContextState::default();
        let mut view = state.view();
        view.window_size_constraints(constraints);
        view.column_start();
        view.column_end();
        let bytes = view.finish();
        let expected = [0.5_f32, 0.0, 0.75, 0.25, 2.0].map(f32::to_le_bytes);
        assert_eq!(&bytes[13..33], expected.as_flattened());
        assert!(
            !WindowSizeConstraints { aspect_ratio: 2.0_f32.next_down(), ..constraints }.is_valid()
        );
    }

    #[test]
    fn scene_storage_is_reused_after_abandonment() {
        // An unfinished frame must not discard storage or affect the next frame's tree.
        let mut state = crate::context::ContextState::default();
        let mut view = state.view();
        view.theme(&[]);
        view.column_start();
        view.text(TextStyle::Body, &"x".repeat(4096));
        let pointer = view.bytes.as_ptr();
        let capacity = view.bytes.capacity();

        let mut view = state.view();
        assert_eq!(view.byte_len(), VIEW_MAGIC.len());
        assert_eq!(view.bytes.as_ptr(), pointer);
        assert_eq!(view.bytes.capacity(), capacity);
        view.theme(&[]);
        view.column_start();
        view.text(TextStyle::Body, "Fresh frame");
        view.column_end();
        let bytes = view.finish();
        assert_eq!(bytes.as_ptr(), pointer);
    }

    #[test]
    fn scroll_and_image_encoder_failures_are_atomic() {
        // A root viewport stays usable after rejected identities, children and images.
        let mut view_state = crate::context::ContextState::default();
        let mut view = view_state.view();
        assert_panics(|| view.scroll_start(0, true));
        view.scroll_start(1, true);
        assert_panics(|| view.scroll_end());
        assert_panics(|| view.jpeg_image(2, 1024, 681, &[], ""));
        view.jpeg_image(2, 1024, 681, &[0; MAX_IMAGE_BYTES], "Photo");
        view.scroll_end();
        view.finish();
    }
    #[test]
    fn named_file_image_wire_is_fixed_and_invalid_references_do_not_write() {
        let file = super::FileReference {
            volume: crate::abi::syscall::VolumeHandle { generation: 42, slot: 3 },
            entry_id: [1; 15],
        };
        let mut encoder_state = crate::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        assert_panics(|| encoder.file_image(file, 0, 1, "Test image"));
        assert_panics(|| encoder.file_image(file, 1, 4097, "Test image"));
        assert_panics(|| {
            encoder.file_image(
                super::FileReference { entry_id: [0; 15], ..file },
                1,
                1,
                "Test image",
            )
        });
        encoder.file_image(file, 320, 240, "Test image");
        encoder.column_end();
        let encoded = encoder.finish();
        assert_eq!(encoded.len(), 62 + 9 + "Test image".len());
        assert_eq!(encoded[13], 20);
        assert_eq!(&encoded[18..22], &31u32.to_le_bytes());
        assert_eq!(&encoded[22..26], &3u32.to_le_bytes());
        assert_eq!(&encoded[26..30], &42u32.to_le_bytes());
        assert_eq!(&encoded[30..45], &[1; 15]);
        assert_eq!(encoded[53], VIEW_IMAGE_DESCRIPTION);
        assert_eq!(&encoded[54..58], &0_u32.to_le_bytes());
        assert_eq!(&encoded[58..62], &("Test image".len() as u32).to_le_bytes());
        assert_eq!(&encoded[62..72], b"Test image");
    }

    use super::*;

    #[test]
    fn light_theme_defines_every_semantic_color() {
        let theme = Theme::light();
        for value in 0..ThemeColor::COUNT as u8 {
            let role = ThemeColor::from_wire(value).unwrap();
            assert_eq!(role as u8, value);
            assert_ne!(theme.color(role).0 & 0xff, 0);
        }
        assert_eq!(theme.color(ThemeColor::DestructivePressed), Color(0xa91700ff));
    }

    #[test]
    fn invalid_theme_colors_are_rejected() {
        for value in ThemeColor::COUNT as u8..=u8::MAX {
            assert_eq!(ThemeColor::from_wire(value), None);
        }
    }

    #[test]
    fn encoder_records_themed_native_controls() {
        let mut view_state = crate::context::ContextState::default();
        let mut view = view_state.view();
        view.theme(&[ThemeOverride {
            role: ThemeColor::PrimaryBackground,
            color: Color(0x112233ff),
        }]);
        view.column_start();
        view.text(TextStyle::Title, "Controls");
        view.row_start();
        view.button(1, "Save", ButtonStyle::Primary, false);
        view.checkbox(2, "Shared", &mut CheckState::Mixed, false);
        view.row_end();
        view.text_field(4, &mut "Ada".into(), "Name", false, false, false);
        view.autocomplete_field(
            9,
            "Cat",
            "Animal",
            &[
                AutocompleteItem { label: "Cat", value: "cat" },
                AutocompleteItem { label: "Dog", value: "dog" },
            ],
            false,
        );
        view.popup_button(
            5,
            "Animals",
            &[
                MenuItem::Action {
                    icon: None,
                    id: 6,
                    label: "Cats",
                    checked: Some(true),
                    disabled: false,
                    destructive: false,
                },
                MenuItem::Separator,
                MenuItem::Submenu {
                    icon: None,
                    label: "More",
                    disabled: false,
                    items: &[MenuItem::Action {
                        icon: None,
                        id: 7,
                        label: "Birds",
                        checked: None,
                        disabled: false,
                        destructive: false,
                    }],
                },
            ],
            false,
        );
        view.context_menu_start(&[MenuItem::Action {
            icon: None,
            id: 10,
            label: "Inspect",
            checked: None,
            disabled: false,
            destructive: false,
        }]);
        view.button(11, "Right-click", ButtonStyle::Default, false);
        view.context_menu_end();
        view.column_end();
        let bytes = view.finish();

        assert_eq!(&bytes[..4], VIEW_MAGIC);
        assert!(bytes.windows(8).any(|window| window == b"Controls"));
        assert!(bytes.windows(6).any(|window| window == b"Shared"));
    }

    #[test]
    fn encoder_rejects_mismatched_containers_and_zero_ids() {
        let mut view_state = crate::context::ContextState::default();
        let mut view = view_state.view();
        view.column_start();
        assert_panics(|| view.row_end());
        assert_panics(|| {
            view.button(0, "Invalid", ButtonStyle::Default, false);
        });

        let item = MenuItem::Action {
            icon: None,
            id: 1,
            label: "Open",
            checked: None,
            disabled: false,
            destructive: false,
        };
        let mut empty_context_state = crate::context::ContextState::default();
        let mut empty_context = empty_context_state.view();
        empty_context.context_menu_start(&[item]);
        assert_panics(|| empty_context.context_menu_end());

        let mut multiple_context_state = crate::context::ContextState::default();
        let mut multiple_context = multiple_context_state.view();
        multiple_context.context_menu_start(&[item]);
        multiple_context.text(TextStyle::Body, "One");
        multiple_context.text(TextStyle::Body, "Two");
        assert_panics(|| multiple_context.context_menu_end());
    }

    #[test]
    fn encoder_grows_and_finish_transfers_the_buffer() {
        // Growth must preserve all earlier commands and return the same allocation.
        let text = "🦋".repeat(1024);
        let mut encoder_state = crate::context::ContextState::default();
        let mut encoder = encoder_state.view();
        let initial_capacity = encoder.bytes.capacity();
        encoder.column_start();
        for _ in 0..128 {
            encoder.text(TextStyle::Body, &text);
        }
        encoder.column_end();
        let pointer = encoder.bytes.as_ptr();
        let capacity = encoder.bytes.capacity();
        assert!(capacity > initial_capacity);
        let encoded = encoder.finish();

        assert_eq!(encoded.as_ptr(), pointer);
        assert_eq!(encoded.capacity(), capacity);
        assert_eq!(encoded.len(), 22 + 128 * (10 + text.len()));
        assert_eq!(&encoded[..13], b"PBV1\x01\0\0\0\0\0\0\0\0");
        assert_eq!(&encoded[encoded.len() - 9..], b"\x04\0\0\0\0\0\0\0\0");
        for command in encoded[13..encoded.len() - 9].chunks_exact(10 + text.len()) {
            assert_eq!(&command[..5], &[VIEW_TEXT, 0, 0, 0, 0]);
            assert_eq!(&command[5..9], &(1 + text.len() as u32).to_le_bytes());
            assert_eq!(command[9], TextStyle::Body as u8);
            assert_eq!(&command[10..], text.as_bytes());
        }
    }

    #[test]
    fn finish_panics_without_exactly_one_closed_root() {
        // Finalization must reject missing, extra, and unfinished roots.
        assert_panics(|| {
            crate::context::ContextState::default().view().finish();
        });

        let mut unclosed_state = crate::context::ContextState::default();
        let mut unclosed = unclosed_state.view();
        unclosed.column_start();
        assert_panics(|| {
            unclosed.finish();
        });

        let mut multiple_state = crate::context::ContextState::default();
        let mut multiple = multiple_state.view();
        for _ in 0..2 {
            multiple.column_start();
            multiple.column_end();
        }
        assert_panics(|| {
            multiple.finish();
        });
    }

    #[test]
    fn encoder_panics_on_invalid_parent_theme_and_depth() {
        // Growing storage must not relax the tree and theme constraints.
        let mut encoder_state = crate::context::ContextState::default();
        let mut encoder = encoder_state.view();
        assert_panics(|| encoder.text(TextStyle::Body, "orphan"));
        assert_panics(|| encoder.column_end());
        encoder.theme(&[]);
        assert_panics(|| encoder.theme(&[]));
        for _ in 0..MAX_CONTAINER_DEPTH {
            encoder.column_start();
        }
        assert_panics(|| encoder.theme(&[]));
        assert_panics(|| encoder.column_start());
        for _ in 0..MAX_CONTAINER_DEPTH {
            encoder.column_end();
        }
        encoder.finish();
    }

    #[test]
    fn encoder_panics_when_command_length_cannot_fit() {
        // A growable allocation cannot make an overflowing wire length valid.
        let mut encoder_state = crate::context::ContextState::default();
        let mut encoder = encoder_state.view();
        assert_panics(|| encoder.reserve_command(usize::MAX));
        assert_eq!(encoder.bytes.as_slice(), VIEW_MAGIC);
    }

    #[test]
    fn image_description_bound_panics_before_writing() {
        let file = FileReference {
            volume: crate::abi::syscall::VolumeHandle { generation: 1, slot: 1 },
            entry_id: [1; 15],
        };
        let mut encoder_state = crate::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        let huge = [b'x'; 4097];
        assert_panics(|| encoder.file_image(file, 1, 1, core::str::from_utf8(&huge).unwrap()));
        assert_eq!(encoder.bytes.len(), 13);
        assert_eq!(encoder.child_counts[0], 0);
        let mut maximum = [0; 4096];
        for chunk in maximum.as_chunks_mut::<4>().0 {
            chunk.copy_from_slice("🦋".as_bytes());
        }
        encoder.file_image(file, 1, 1, core::str::from_utf8(&maximum).unwrap());
        encoder.column_end();
        let encoded = encoder.finish();
        assert_eq!(&encoded[62..4158], &maximum);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests may use unwrap as an assert.")]
mod segment_tests {
    use super::tests::assert_panics;
    use super::*;
    #[test]
    fn malformed_segment_groups_do_not_partially_write() {
        let mut encoder_state = crate::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        let before = encoder.bytes.len();
        let mut segments = [
            Segment { id: 2, label: "A", disabled: false },
            Segment { id: 3, label: "B", disabled: false },
            Segment { id: 4, label: "C", disabled: false },
        ];
        assert_panics(|| {
            encoder.segmented_control(1, &segments[..2], 2, false);
        });
        assert_panics(|| {
            encoder.segmented_control(1, &segments, 99, false);
        });
        for id in [0, 1, 2] {
            segments[1].id = id;
            assert_panics(|| {
                encoder.segmented_control(1, &segments, 2, false);
            });
        }
        assert_eq!(encoder.bytes.len(), before);
        let too_many = [Segment { id: 2, label: "", disabled: false }; MAX_SEGMENTS + 1];
        assert_panics(|| {
            encoder.segmented_control(1, &too_many, 2, false);
        });
        assert_eq!(encoder.bytes.len(), before);
    }
}
