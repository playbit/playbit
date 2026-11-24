//! Bounded semantic scenes shared by app execution and host presentation.
//!
//! Decoding validates the scene format, record/depth limits and event IDs.
//! File authority is checked by the caller before a scene is accepted. This
//! module owns no VM, guest memory, store, event queue or platform renderer.
//! Scene decoding is control plane; limits and validation run in release.

use std::collections::BTreeSet;

use playbit_api::ui::{
    ButtonStyle, CONTROL_DISABLED, CanvasExtent, CanvasFrames, CanvasOptions, CheckState, Color,
    FileReference, LIST_HAS_CHILDREN, LIST_INACTIVE, LIST_SELECTED, MAX_IMAGE_BYTES, MAX_SEGMENTS,
    MENU_ACTION, MENU_CHECKABLE, MENU_CHECKED, MENU_DESTRUCTIVE, MENU_SEPARATOR, MENU_SUBMENU,
    MIN_SEGMENTS, MenuIcon, TEXT_FIELD_MULTILINE, TEXT_FIELD_READ_ONLY, TextStyle, Theme,
    ThemeColor, VIEW_AUTOCOMPLETE_FIELD, VIEW_BUTTON, VIEW_CANVAS, VIEW_CHECKBOX, VIEW_COLUMN_END,
    VIEW_COLUMN_START, VIEW_CONTEXT_MENU_END, VIEW_CONTEXT_MENU_START, VIEW_FILE_IMAGE,
    VIEW_IMAGE_DESCRIPTION, VIEW_JPEG_IMAGE, VIEW_LIST_END, VIEW_LIST_ROW, VIEW_LIST_START,
    VIEW_MAGIC, VIEW_POPUP_BUTTON, VIEW_RADIO, VIEW_ROW_END, VIEW_ROW_START, VIEW_SCROLL_END,
    VIEW_SCROLL_START, VIEW_SEGMENTED_CONTROL, VIEW_TEXT, VIEW_TEXT_FIELD, VIEW_THEME,
    VIEW_WINDOW_SIZE_CONSTRAINTS, VIEW_WINDOW_STYLE, WindowSizeConstraints, WindowStyle,
    image_size_valid, segment_ids_valid,
};

const MAX_VIEW_DEPTH: usize = 32;
const MAX_MENU_DEPTH: usize = 8;
const MAX_VIEW_NODES: usize = 2048;
const _: () = assert!(MAX_MENU_DEPTH <= MAX_VIEW_DEPTH);
const _: () = assert!(MAX_VIEW_DEPTH * 2 <= MAX_VIEW_NODES);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanvasItem {
    Rect(playbit_api::ui::CanvasRect),
    Text {
        bounds: playbit_api::ui::CanvasRect,
        font_size: u32,
        bold: bool,
        text: String,
    },
    Pixels {
        bounds: playbit_api::ui::CanvasRect,
        source_width: u32,
        source_height: u32,
        rgba: std::sync::Arc<[u8]>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ViewNode {
    Theme {
        theme: Theme,
        window_background_specified: bool,
        window_style: Option<WindowStyle>,
        window_size_constraints: Option<WindowSizeConstraints>,
        child: Box<ViewNode>,
    },
    Scroll {
        id: u32,
        complex_background: bool,
        child: Box<ViewNode>,
    },
    JpegImage {
        id: u32,
        width: u32,
        height: u32,
        jpeg: Vec<u8>,
        label: String,
    },
    Column(Vec<ViewNode>),
    Row(Vec<ViewNode>),
    List(Vec<ViewNode>),
    ContextMenu {
        items: Vec<MenuEntry>,
        child: Box<ViewNode>,
    },
    Text {
        style: TextStyle,
        text: String,
    },
    Canvas {
        id: u32,
        width: u32,
        height: u32,
        frames: CanvasFrames,
        extent: CanvasExtent,
        clear_color: Option<Color>,
        items: Vec<CanvasItem>,
    },
    FileImage {
        file: FileReference,
        width: u32,
        height: u32,
        /// None is unspecified; an empty label marks a decorative image.
        label: Option<String>,
    },
    TextField {
        id: u32,
        value: String,
        placeholder: String,
        multiline: bool,
        disabled: bool,
        read_only: bool,
    },
    AutocompleteField {
        id: u32,
        value: String,
        placeholder: String,
        suggestions: Vec<AutocompleteEntry>,
        disabled: bool,
    },
    Button {
        id: u32,
        label: String,
        style: ButtonStyle,
        disabled: bool,
    },
    Checkbox {
        id: u32,
        label: String,
        state: CheckState,
        disabled: bool,
    },
    Radio {
        id: u32,
        label: String,
        selected: bool,
        disabled: bool,
    },
    SegmentedControl {
        id: u32,
        segments: Vec<SegmentEntry>,
        selected: u32,
        disabled: bool,
    },
    PopupButton {
        id: u32,
        label: String,
        items: Vec<MenuEntry>,
        disabled: bool,
    },
    ListRow {
        id: u32,
        label: String,
        selected: bool,
        inactive: bool,
        has_children: bool,
        disabled: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentEntry {
    pub id: u32,
    pub label: String,
    pub disabled: bool,
}

/// Guest symbols retain their wire identities; host menus may use bundled artwork.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MenuGlyph {
    Builtin(MenuIcon),
    Asset(&'static str),
}

impl From<MenuIcon> for MenuGlyph {
    fn from(icon: MenuIcon) -> Self {
        Self::Builtin(icon)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum MenuEntry {
    Action {
        id: u32,
        label: String,
        icon: Option<MenuGlyph>,
        checked: Option<bool>,
        disabled: bool,
        destructive: bool,
    },
    Separator,
    Submenu {
        label: String,
        icon: Option<MenuGlyph>,
        items: Vec<MenuEntry>,
        disabled: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutocompleteEntry {
    pub label: String,
    pub value: String,
}

/// Decode the bounded scene ABI. This validates encoding, not data authority.
#[allow(
    clippy::result_unit_err,
    reason = "Malformed scene input has one deliberately opaque failure result."
)]
// fuzz: decode_view
pub fn decode_view(bytes: &[u8]) -> Result<ViewNode, ()> {
    if bytes.len() > playbit_api::abi::syscall::MAX_CALL_BYTES || !bytes.starts_with(VIEW_MAGIC) {
        return Err(());
    }
    let mut position = VIEW_MAGIC.len();
    let mut decoder = ViewDecoder {
        containers: Vec::new(),
        root: None,
        node_count: 0,
        theme: Theme::light(),
        theme_seen: false,
        window_style: None,
        window_size_constraints: None,
        theme_roles: [false; ThemeColor::COUNT],
        canvas_items: 0,
        canvas_text_bytes: 0,
        canvas_pixel_bytes: 0,
    };
    let mut previous_kind = None;
    while position < bytes.len() {
        let kind = take(bytes, &mut position, 1)?[0];
        let id = read_u32(bytes, &mut position)?;
        let payload_length = read_u32(bytes, &mut position)? as usize;
        let payload = take(bytes, &mut position, payload_length)?;
        decoder.node_count += 1;
        if decoder.node_count > MAX_VIEW_NODES {
            return Err(());
        }
        assert!(decoder.node_count <= MAX_VIEW_NODES);
        decoder.decode_record(kind, id, payload, previous_kind.replace(kind))?;
        assert!(decoder.containers.len() <= MAX_VIEW_DEPTH);
    }
    if !decoder.containers.is_empty() {
        return Err(());
    }
    assert_eq!(position, bytes.len());
    let view = ViewNode::Theme {
        theme: decoder.theme,
        window_background_specified: decoder.theme_roles[ThemeColor::WindowBackground as usize],
        window_style: decoder.window_style,
        window_size_constraints: decoder.window_size_constraints,
        child: Box::new(decoder.root.ok_or(())?),
    };
    validate_view_ids(&view)?;
    assert_eq!(view_records(&view, 0) + usize::from(decoder.theme_seen), decoder.node_count);
    Ok(view)
}

struct ViewDecoder {
    containers: Vec<ViewContainer>,
    root: Option<ViewNode>,
    node_count: usize,
    theme: Theme,
    theme_seen: bool,
    window_style: Option<WindowStyle>,
    window_size_constraints: Option<WindowSizeConstraints>,
    theme_roles: [bool; ThemeColor::COUNT],
    canvas_items: usize,
    canvas_text_bytes: usize,
    canvas_pixel_bytes: usize,
}

impl ViewDecoder {
    // fuzz: decode_view
    fn decode_record(
        &mut self,
        kind: u8,
        id: u32,
        payload: &[u8],
        preceding: Option<u8>,
    ) -> Result<(), ()> {
        assert!(self.containers.len() <= MAX_VIEW_DEPTH);
        assert!(self.node_count <= MAX_VIEW_NODES);
        match kind {
            VIEW_WINDOW_STYLE | VIEW_WINDOW_SIZE_CONSTRAINTS
                if id == 0 && self.root.is_none() && self.containers.is_empty() =>
            {
                self.decode_window_metadata(kind, payload)
            }
            VIEW_THEME
                if id == 0
                    && !self.theme_seen
                    && self.root.is_none()
                    && self.containers.is_empty() =>
            {
                self.decode_theme(payload)
            }
            VIEW_COLUMN_START | VIEW_ROW_START | VIEW_LIST_START
                if id == 0 && payload.is_empty() =>
            {
                self.decode_container_start(kind)
            }
            VIEW_SCROLL_START if id != 0 => self.decode_scroll_start(id, payload),
            VIEW_JPEG_IMAGE if id != 0 => self.decode_jpeg_image(id, payload),
            VIEW_CONTEXT_MENU_START if id == 0 => self.decode_context_menu_start(kind, payload),
            VIEW_COLUMN_END
            | VIEW_ROW_END
            | VIEW_LIST_END
            | VIEW_CONTEXT_MENU_END
            | VIEW_SCROLL_END
                if id == 0 && payload.is_empty() =>
            {
                self.decode_container_end(kind)
            }
            VIEW_TEXT if id == 0 => self.decode_text(payload),
            VIEW_CANVAS => self.decode_canvas(id, payload),
            VIEW_FILE_IMAGE if id == 0 && payload.len() == 31 => self.decode_file_image(payload),
            VIEW_IMAGE_DESCRIPTION if id == 0 && preceding == Some(VIEW_FILE_IMAGE) => {
                self.decode_image_description(payload)
            }
            VIEW_BUTTON if id != 0 => self.decode_button(id, payload),
            VIEW_TEXT_FIELD if id != 0 => self.decode_text_field(id, payload),
            VIEW_AUTOCOMPLETE_FIELD if id != 0 => self.decode_autocomplete_field(id, payload),
            VIEW_CHECKBOX if id != 0 => self.decode_checkbox(id, payload),
            VIEW_RADIO if id != 0 => self.decode_radio(id, payload),
            VIEW_SEGMENTED_CONTROL if id != 0 => self.decode_segmented_control(id, payload),
            VIEW_POPUP_BUTTON if id != 0 => self.decode_popup_button(id, payload),
            VIEW_LIST_ROW if id != 0 => self.decode_list_row(id, payload),
            _ => Err(()),
        }
    }
    // fuzz: decode_view
    fn decode_window_metadata(&mut self, kind: u8, payload: &[u8]) -> Result<(), ()> {
        match kind {
            VIEW_WINDOW_STYLE if self.window_style.is_none() => {
                let bits = u32::from_le_bytes(payload.try_into().map_err(|_| ())?);
                self.window_style = Some(WindowStyle::from_bits(bits).ok_or(())?);
            }
            VIEW_WINDOW_SIZE_CONSTRAINTS if self.window_size_constraints.is_none() => {
                let (words, []) = payload.as_chunks::<4>() else {
                    return Err(());
                };
                let &[min_width, min_height, max_width, max_height, ratio] = words else {
                    return Err(());
                };
                let constraints = WindowSizeConstraints {
                    min_width: f32::from_le_bytes(min_width),
                    min_height: f32::from_le_bytes(min_height),
                    max_width: f32::from_le_bytes(max_width),
                    max_height: f32::from_le_bytes(max_height),
                    aspect_ratio: f32::from_le_bytes(ratio),
                };
                if !constraints.is_valid() {
                    return Err(());
                }
                self.window_size_constraints = Some(constraints);
            }
            _ => return Err(()),
        }
        Ok(())
    }

    // fuzz: decode_view
    fn decode_theme(&mut self, payload: &[u8]) -> Result<(), ()> {
        if !payload.len().is_multiple_of(5) {
            return Err(());
        }
        let mut payload_position = 0;
        while payload_position < payload.len() {
            let role =
                ThemeColor::from_wire(take(payload, &mut payload_position, 1)?[0]).ok_or(())?;
            if self.theme_roles[role as usize] {
                return Err(());
            }
            self.theme_roles[role as usize] = true;
            let color = u32::from_le_bytes(
                take(payload, &mut payload_position, 4)?.try_into().map_err(|_| ())?,
            );
            self.theme.set_color(role, playbit_api::ui::Color(color));
        }
        self.theme_seen = true;
        assert!(self.theme_seen);
        assert_eq!(self.theme_roles.iter().filter(|set| **set).count(), payload.len() / 5);
        Ok(())
    }
    // fuzz: decode_view
    fn decode_container_start(&mut self, kind: u8) -> Result<(), ()> {
        if self.root.is_some() || self.containers.len() >= MAX_VIEW_DEPTH {
            return Err(());
        }
        self.containers.push((kind, Vec::new(), None, None));
        assert!(self.root.is_none());
        assert_eq!(self.containers.last().map(|c| c.0), Some(kind));
        Ok(())
    }
    // fuzz: decode_view
    fn decode_context_menu_start(&mut self, kind: u8, payload: &[u8]) -> Result<(), ()> {
        if self.root.is_some() || self.containers.len() >= MAX_VIEW_DEPTH {
            return Err(());
        }
        let mut payload_position = 0;
        let item_count = read_u32(payload, &mut payload_position)? as usize;
        if item_count == 0 {
            return Err(());
        }
        let items = decode_menu_entries(
            payload,
            &mut payload_position,
            item_count,
            0,
            &mut self.node_count,
        )?;
        if payload_position != payload.len() {
            return Err(());
        }
        self.containers.push((kind, Vec::new(), Some(items), None));
        assert!(self.root.is_none());
        assert!(self.containers.last().is_some_and(|c| c.2.is_some()));
        Ok(())
    }
    // fuzz: decode_view
    fn decode_container_end(&mut self, kind: u8) -> Result<(), ()> {
        let (start, mut children, menu, scroll) = self.containers.pop().ok_or(())?;
        let container = match (start, kind) {
            (VIEW_COLUMN_START, VIEW_COLUMN_END) => ViewNode::Column(children),
            (VIEW_ROW_START, VIEW_ROW_END) => ViewNode::Row(children),
            (VIEW_LIST_START, VIEW_LIST_END) => ViewNode::List(children),
            (VIEW_SCROLL_START, VIEW_SCROLL_END) => {
                if children.len() != 1 {
                    return Err(());
                }
                let (id, complex_background) = scroll.ok_or(())?;
                ViewNode::Scroll {
                    id,
                    complex_background,
                    child: Box::new(children.pop().ok_or(())?),
                }
            }
            (VIEW_CONTEXT_MENU_START, VIEW_CONTEXT_MENU_END) => {
                if children.len() != 1 {
                    return Err(());
                }
                ViewNode::ContextMenu {
                    items: menu.ok_or(())?,
                    child: Box::new(children.pop().ok_or(())?),
                }
            }
            _ => return Err(()),
        };
        push_view_node(&mut self.containers, &mut self.root, container)?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_scroll_start(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        if self.root.is_some() || self.containers.len() >= MAX_VIEW_DEPTH || payload.len() != 1 {
            return Err(());
        }
        if payload[0] > 1 {
            return Err(());
        }
        self.containers.push((VIEW_SCROLL_START, Vec::new(), None, Some((id, payload[0] == 1))));
        assert!(self.containers.len() <= MAX_VIEW_DEPTH);
        Ok(())
    }
    // fuzz: decode_view
    fn decode_jpeg_image(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let mut position = 0;
        let width = read_u32(payload, &mut position)?;
        let height = read_u32(payload, &mut position)?;
        let label_len = read_u32(payload, &mut position)? as usize;
        if !image_size_valid(width, height) || label_len > 4096 {
            return Err(());
        }
        let label =
            std::str::from_utf8(take(payload, &mut position, label_len)?).map_err(|_| ())?;
        let jpeg = &payload[position..];
        if jpeg.is_empty() || jpeg.len() > MAX_IMAGE_BYTES {
            return Err(());
        }
        push_child(
            &mut self.containers,
            ViewNode::JpegImage { id, width, height, label: label.to_owned(), jpeg: jpeg.to_vec() },
        )?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_text(&mut self, payload: &[u8]) -> Result<(), ()> {
        let (&style, text) = payload.split_first().ok_or(())?;
        let style = TextStyle::from_wire(style).ok_or(())?;
        let text = std::str::from_utf8(text).map_err(|_| ())?.to_owned();
        push_child(&mut self.containers, ViewNode::Text { style, text })?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_canvas(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let mut reader = playbit_api::abi::syscall::Reader::new(payload).map_err(|_| ())?;
        let width = reader.u32().map_err(|_| ())?;
        let height = reader.u32().map_err(|_| ())?;
        let flags = reader.u32().map_err(|_| ())?;
        let count = reader
            .count((playbit_api::ui::MAX_CANVAS_ITEMS - self.canvas_items) as u32)
            .map_err(|_| ())?;
        let CanvasOptions { frames, extent, clear_color } =
            decode_canvas_options(flags, &mut reader)?;
        if extent != CanvasExtent::Inline
            && (id == 0 && frames == CanvasFrames::Continuous
                || self.containers.iter().any(|entry| entry.0 != VIEW_CONTEXT_MENU_START))
        {
            return Err(());
        }
        let edges = 1..=extent.max_edge();
        if !edges.contains(&width) || !edges.contains(&height) {
            return Err(());
        }
        let mut items = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let kind = reader.u32().map_err(|_| ())?;
            let rect = playbit_api::ui::CanvasRect {
                x: reader.u32().map_err(|_| ())?,
                y: reader.u32().map_err(|_| ())?,
                width: reader.u32().map_err(|_| ())?,
                height: reader.u32().map_err(|_| ())?,
                color: playbit_api::ui::Color(reader.u32().map_err(|_| ())?),
            };
            if !rect.fits(width, height) {
                return Err(());
            }
            items.push(self.decode_canvas_item(kind, rect, width, height, &mut reader)?);
        }
        reader.finish().map_err(|_| ())?;
        self.canvas_items += items.len();
        let canvas = ViewNode::Canvas { id, width, height, frames, extent, clear_color, items };
        if extent != CanvasExtent::Inline {
            push_view_node(&mut self.containers, &mut self.root, canvas)?;
        } else {
            push_child(&mut self.containers, canvas)?;
        }
        Ok(())
    }
    // fuzz: decode_view
    fn decode_canvas_item(
        &mut self,
        kind: u32,
        rect: playbit_api::ui::CanvasRect,
        width: u32,
        height: u32,
        reader: &mut playbit_api::abi::syscall::Reader<'_>,
    ) -> Result<CanvasItem, ()> {
        match kind {
            0 => Ok(CanvasItem::Rect(rect)),
            1 => {
                let font_size = reader.u32().map_err(|_| ())?;
                let bold = match reader.u32().map_err(|_| ())? {
                    0 => false,
                    1 => true,
                    _ => return Err(()),
                };
                let text = reader.string().map_err(|_| ())?;
                let item = playbit_api::ui::CanvasText { bounds: rect, font_size, bold, text };
                if !item.fits(width, height)
                    || text.len() > playbit_api::ui::MAX_CANVAS_TEXT_BYTES - self.canvas_text_bytes
                {
                    return Err(());
                }
                self.canvas_text_bytes += text.len();
                Ok(CanvasItem::Text { bounds: rect, font_size, bold, text: text.to_owned() })
            }
            2 => {
                let source_width = reader.u32().map_err(|_| ())?;
                let source_height = reader.u32().map_err(|_| ())?;
                let length = reader.u32().map_err(|_| ())? as usize;
                if length > playbit_api::ui::MAX_CANVAS_PIXEL_BYTES - self.canvas_pixel_bytes {
                    return Err(());
                }
                let rgba = reader.take(length).map_err(|_| ())?;
                let item = playbit_api::ui::CanvasPixels {
                    bounds: rect,
                    source_width,
                    source_height,
                    rgba,
                };
                if !item.fits(width, height) {
                    return Err(());
                }
                self.canvas_pixel_bytes += length;
                Ok(CanvasItem::Pixels {
                    bounds: rect,
                    source_width,
                    source_height,
                    rgba: rgba.into(),
                })
            }
            _ => Err(()),
        }
    }
    // fuzz: decode_view
    fn decode_file_image(&mut self, payload: &[u8]) -> Result<(), ()> {
        let mut reader = playbit_api::abi::syscall::Reader::new(payload).map_err(|_| ())?;
        let file = FileReference {
            volume: reader.handle().map_err(|_| ())?,
            entry_id: reader.take(15).map_err(|_| ())?.try_into().map_err(|_| ())?,
        };
        let width = reader.u32().map_err(|_| ())?;
        let height = reader.u32().map_err(|_| ())?;
        if file.volume.generation == 0
            || file.volume.slot == 0
            || file.entry_id == [0; 15]
            || width == 0
            || height == 0
            || width > 4096
            || height > 4096
        {
            return Err(());
        }
        push_child(&mut self.containers, ViewNode::FileImage { file, width, height, label: None })?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_image_description(&mut self, payload: &[u8]) -> Result<(), ()> {
        if payload.len() > playbit_api::abi::syscall::MAX_STRING_BYTES {
            return Err(());
        }
        let text = std::str::from_utf8(payload).map_err(|_| ())?;
        let Some((_, children, _, _)) = self.containers.last_mut() else {
            return Err(());
        };
        let Some(ViewNode::FileImage { label, .. }) = children.last_mut() else {
            return Err(());
        };
        if label.is_some() {
            return Err(());
        }
        *label = Some(text.to_owned());
        assert_eq!(label.as_deref(), Some(text));
        assert!(
            label
                .as_ref()
                .is_some_and(|label| label.len() <= playbit_api::abi::syscall::MAX_STRING_BYTES)
        );
        Ok(())
    }
    // fuzz: decode_view
    fn decode_button(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let (&style, payload) = payload.split_first().ok_or(())?;
        let (&flags, label) = payload.split_first().ok_or(())?;
        if flags & !CONTROL_DISABLED != 0 {
            return Err(());
        }
        let style = ButtonStyle::from_wire(style).ok_or(())?;
        let label = std::str::from_utf8(label).map_err(|_| ())?.to_owned();
        push_child(
            &mut self.containers,
            ViewNode::Button { id, label, style, disabled: flags & CONTROL_DISABLED != 0 },
        )?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_text_field(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let mut payload_position = 0;
        let flags = take(payload, &mut payload_position, 1)?[0];
        if flags & !(CONTROL_DISABLED | TEXT_FIELD_MULTILINE | TEXT_FIELD_READ_ONLY) != 0 {
            return Err(());
        }
        let value_length = read_u32(payload, &mut payload_position)? as usize;
        let value = take(payload, &mut payload_position, value_length)?;
        let remaining = payload.len().checked_sub(payload_position).ok_or(())?;
        let placeholder = take(payload, &mut payload_position, remaining)?;
        let value = std::str::from_utf8(value).map_err(|_| ())?.to_owned();
        let placeholder = std::str::from_utf8(placeholder).map_err(|_| ())?.to_owned();
        push_child(
            &mut self.containers,
            ViewNode::TextField {
                id,
                value,
                placeholder,
                multiline: flags & TEXT_FIELD_MULTILINE != 0,
                disabled: flags & CONTROL_DISABLED != 0,
                read_only: flags & TEXT_FIELD_READ_ONLY != 0,
            },
        )?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_autocomplete_field(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let mut payload_position = 0;
        let flags = take(payload, &mut payload_position, 1)?[0];
        if flags & !CONTROL_DISABLED != 0 {
            return Err(());
        }
        let value_length = read_u32(payload, &mut payload_position)? as usize;
        let placeholder_length = read_u32(payload, &mut payload_position)? as usize;
        let suggestion_count = read_u32(payload, &mut payload_position)? as usize;
        if suggestion_count == 0
            || suggestion_count > MAX_VIEW_NODES.saturating_sub(self.node_count)
        {
            return Err(());
        }
        let value = take(payload, &mut payload_position, value_length)?;
        let placeholder = take(payload, &mut payload_position, placeholder_length)?;
        let value = std::str::from_utf8(value).map_err(|_| ())?.to_owned();
        let placeholder = std::str::from_utf8(placeholder).map_err(|_| ())?.to_owned();
        let mut suggestions = Vec::with_capacity(suggestion_count);
        for _ in 0..suggestion_count {
            self.node_count = self.node_count.checked_add(1).ok_or(())?;
            if self.node_count > MAX_VIEW_NODES {
                return Err(());
            }
            assert!(self.node_count <= MAX_VIEW_NODES);
            let label_length = read_u32(payload, &mut payload_position)? as usize;
            let suggestion_value_length = read_u32(payload, &mut payload_position)? as usize;
            let label = take(payload, &mut payload_position, label_length)?;
            let suggestion_value = take(payload, &mut payload_position, suggestion_value_length)?;
            suggestions.push(AutocompleteEntry {
                label: std::str::from_utf8(label).map_err(|_| ())?.to_owned(),
                value: std::str::from_utf8(suggestion_value).map_err(|_| ())?.to_owned(),
            });
        }
        if payload_position != payload.len() {
            return Err(());
        }
        push_child(
            &mut self.containers,
            ViewNode::AutocompleteField {
                id,
                value,
                placeholder,
                suggestions,
                disabled: flags & CONTROL_DISABLED != 0,
            },
        )?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_checkbox(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let (&state, payload) = payload.split_first().ok_or(())?;
        let (&flags, label) = payload.split_first().ok_or(())?;
        if flags & !CONTROL_DISABLED != 0 {
            return Err(());
        }
        let state = CheckState::from_wire(state).ok_or(())?;
        let label = std::str::from_utf8(label).map_err(|_| ())?.to_owned();
        push_child(
            &mut self.containers,
            ViewNode::Checkbox { id, label, state, disabled: flags & CONTROL_DISABLED != 0 },
        )?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_radio(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let (&selected, payload) = payload.split_first().ok_or(())?;
        let (&flags, label) = payload.split_first().ok_or(())?;
        if selected > 1 || flags & !CONTROL_DISABLED != 0 {
            return Err(());
        }
        let label = std::str::from_utf8(label).map_err(|_| ())?.to_owned();
        push_child(
            &mut self.containers,
            ViewNode::Radio {
                id,
                label,
                selected: selected != 0,
                disabled: flags & CONTROL_DISABLED != 0,
            },
        )?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_segmented_control(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let mut position = 0;
        let flags = take(payload, &mut position, 1)?[0];
        if flags & !CONTROL_DISABLED != 0 {
            return Err(());
        }
        let selected = read_u32(payload, &mut position)?;
        let count = take(payload, &mut position, 1)?[0] as usize;
        if !(MIN_SEGMENTS..=MAX_SEGMENTS).contains(&count)
            || count > MAX_VIEW_NODES - self.node_count
        {
            return Err(());
        }
        self.node_count += count;
        let mut ids = [0; MAX_SEGMENTS];
        let mut segments = Vec::with_capacity(count);
        for id in &mut ids[..count] {
            *id = read_u32(payload, &mut position)?;
            let flags = take(payload, &mut position, 1)?[0];
            if flags & !CONTROL_DISABLED != 0 {
                return Err(());
            }
            let length = read_u32(payload, &mut position)? as usize;
            let label = std::str::from_utf8(take(payload, &mut position, length)?)
                .map_err(|_| ())?
                .to_owned();
            segments.push(SegmentEntry { id: *id, label, disabled: flags & CONTROL_DISABLED != 0 });
        }
        if position != payload.len() || !segment_ids_valid(id, selected, &ids[..count]) {
            return Err(());
        }
        push_child(
            &mut self.containers,
            ViewNode::SegmentedControl {
                id,
                segments,
                selected,
                disabled: flags & CONTROL_DISABLED != 0,
            },
        )
    }
    // fuzz: decode_view
    fn decode_popup_button(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        let mut payload_position = 0;
        let flags = take(payload, &mut payload_position, 1)?[0];
        if flags & !CONTROL_DISABLED != 0 {
            return Err(());
        }
        let label_length = read_u32(payload, &mut payload_position)? as usize;
        let label = take(payload, &mut payload_position, label_length)?;
        let label = std::str::from_utf8(label).map_err(|_| ())?.to_owned();
        let item_count = read_u32(payload, &mut payload_position)? as usize;
        if item_count == 0 {
            return Err(());
        }
        let items = decode_menu_entries(
            payload,
            &mut payload_position,
            item_count,
            0,
            &mut self.node_count,
        )?;
        if payload_position != payload.len() {
            return Err(());
        }
        push_child(
            &mut self.containers,
            ViewNode::PopupButton { id, label, items, disabled: flags & CONTROL_DISABLED != 0 },
        )?;
        Ok(())
    }
    // fuzz: decode_view
    fn decode_list_row(&mut self, id: u32, payload: &[u8]) -> Result<(), ()> {
        if self.containers.last().map(|container| container.0) != Some(VIEW_LIST_START) {
            return Err(());
        }
        let (&flags, label) = payload.split_first().ok_or(())?;
        if flags & !(CONTROL_DISABLED | LIST_SELECTED | LIST_INACTIVE | LIST_HAS_CHILDREN) != 0
            || flags & LIST_INACTIVE != 0 && flags & LIST_SELECTED == 0
        {
            return Err(());
        }
        let label = std::str::from_utf8(label).map_err(|_| ())?.to_owned();
        push_child(
            &mut self.containers,
            ViewNode::ListRow {
                id,
                label,
                selected: flags & LIST_SELECTED != 0,
                inactive: flags & LIST_INACTIVE != 0,
                has_children: flags & LIST_HAS_CHILDREN != 0,
                disabled: flags & CONTROL_DISABLED != 0,
            },
        )?;
        Ok(())
    }
}

// fuzz: decode_view
fn decode_canvas_options(
    flags: u32,
    reader: &mut playbit_api::abi::syscall::Reader<'_>,
) -> Result<CanvasOptions, ()> {
    use playbit_api::ui::{
        CANVAS_FLAG_CLEAR, CANVAS_FLAG_CONTINUOUS, CANVAS_FLAG_VIEWPORT, CANVAS_FLAG_WINDOW,
    };
    let extent = match flags & !(CANVAS_FLAG_CONTINUOUS | CANVAS_FLAG_CLEAR) {
        0 => CanvasExtent::Inline,
        CANVAS_FLAG_VIEWPORT => CanvasExtent::Viewport,
        CANVAS_FLAG_WINDOW => CanvasExtent::Window,
        _ => return Err(()),
    };
    let frames = if flags & CANVAS_FLAG_CONTINUOUS != 0 {
        CanvasFrames::Continuous
    } else {
        CanvasFrames::Static
    };
    let clear_color = if flags & CANVAS_FLAG_CLEAR != 0 {
        Some(Color(reader.u32().map_err(|_| ())?))
    } else {
        None
    };
    Ok(CanvasOptions { frames, extent, clear_color })
}

// Recount the resulting tree independently in wire records: container ends,
// image descriptions and nested menu/suggestion entries also consume budget.
fn view_records(view: &ViewNode, depth: usize) -> usize {
    assert!(depth <= MAX_VIEW_DEPTH + 1); // Includes the implicit Theme wrapper.
    let count = match view {
        ViewNode::Theme { child, window_style, window_size_constraints, .. } => {
            usize::from(window_style.is_some())
                + usize::from(window_size_constraints.is_some())
                + view_records(child, depth + 1)
        }
        ViewNode::Scroll { child, .. } => 2 + view_records(child, depth + 1),
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            2 + children.iter().map(|child| view_records(child, depth + 1)).sum::<usize>()
        }
        ViewNode::ContextMenu { child, items } => {
            2 + view_records(child, depth + 1) + menu_records(items, 0)
        }
        ViewNode::PopupButton { items, .. } => 1 + menu_records(items, 0),
        ViewNode::SegmentedControl { segments, .. } => 1 + segments.len(),
        ViewNode::AutocompleteField { suggestions, .. } => 1 + suggestions.len(),
        ViewNode::FileImage { label, .. } => 1 + usize::from(label.is_some()),
        _ => 1,
    };
    assert!(count > 0);
    assert!(count <= MAX_VIEW_NODES);
    count
}

fn menu_records(items: &[MenuEntry], depth: usize) -> usize {
    assert!(depth < MAX_MENU_DEPTH);
    assert!(items.len() <= MAX_VIEW_NODES);
    let count = items
        .iter()
        .map(|item| match item {
            MenuEntry::Submenu { items, .. } => 1 + menu_records(items, depth + 1),
            _ => 1,
        })
        .sum::<usize>();
    assert!(count >= items.len());
    assert!(count <= MAX_VIEW_NODES);
    count
}

// fuzz: decode_view
fn decode_menu_action(bytes: &[u8], position: &mut usize) -> Result<MenuEntry, ()> {
    let id = read_u32(bytes, position)?;
    let flags = take(bytes, position, 1)?[0];
    if id == 0
        || flags & !(CONTROL_DISABLED | MENU_CHECKABLE | MENU_CHECKED | MENU_DESTRUCTIVE) != 0
        || (flags & MENU_CHECKED != 0 && flags & MENU_CHECKABLE == 0)
    {
        return Err(());
    }
    let icon = take(bytes, position, 1)?[0];
    let icon = if icon == 0 { None } else { Some(MenuIcon::from_byte(icon).ok_or(())?.into()) };
    let label_length = read_u32(bytes, position)? as usize;
    let label = take(bytes, position, label_length)?;
    let label = std::str::from_utf8(label).map_err(|_| ())?.to_owned();
    Ok(MenuEntry::Action {
        id,
        label,
        icon,
        checked: (flags & MENU_CHECKABLE != 0).then_some(flags & MENU_CHECKED != 0),
        disabled: flags & CONTROL_DISABLED != 0,
        destructive: flags & MENU_DESTRUCTIVE != 0,
    })
}

fn decode_menu_entries(
    bytes: &[u8],
    position: &mut usize,
    count: usize,
    depth: usize,
    node_count: &mut usize,
) -> Result<Vec<MenuEntry>, ()> {
    assert!(depth <= MAX_MENU_DEPTH);
    assert!(*node_count <= MAX_VIEW_NODES);
    if depth >= MAX_MENU_DEPTH || count > MAX_VIEW_NODES.saturating_sub(*node_count) {
        return Err(());
    }
    assert!(depth < MAX_MENU_DEPTH);
    assert!(*position <= bytes.len());
    assert!(*node_count <= MAX_VIEW_NODES);
    let before = *node_count;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        *node_count = node_count.checked_add(1).ok_or(())?;
        if *node_count > MAX_VIEW_NODES {
            return Err(());
        }
        assert!(*node_count <= MAX_VIEW_NODES);
        match take(bytes, position, 1)?[0] {
            MENU_ACTION => items.push(decode_menu_action(bytes, position)?),
            MENU_SEPARATOR => items.push(MenuEntry::Separator),
            MENU_SUBMENU => {
                let flags = take(bytes, position, 1)?[0];
                if flags & !CONTROL_DISABLED != 0 {
                    return Err(());
                }
                let icon = take(bytes, position, 1)?[0];
                let icon = if icon == 0 {
                    None
                } else {
                    Some(MenuIcon::from_byte(icon).ok_or(())?.into())
                };
                let label_length = read_u32(bytes, position)? as usize;
                let label = take(bytes, position, label_length)?;
                let label = std::str::from_utf8(label).map_err(|_| ())?.to_owned();
                let child_count = read_u32(bytes, position)? as usize;
                if child_count == 0 {
                    return Err(());
                }
                let children =
                    decode_menu_entries(bytes, position, child_count, depth + 1, node_count)?;
                items.push(MenuEntry::Submenu {
                    icon,
                    label,
                    items: children,
                    disabled: flags & CONTROL_DISABLED != 0,
                });
            }
            _ => return Err(()),
        }
    }
    assert_eq!(items.len(), count);
    assert_eq!(menu_records(&items, depth), *node_count - before);
    assert!(*position <= bytes.len());
    Ok(items)
}

type ViewContainer = (u8, Vec<ViewNode>, Option<Vec<MenuEntry>>, Option<(u32, bool)>);

fn push_child(containers: &mut [ViewContainer], node: ViewNode) -> Result<(), ()> {
    assert!(containers.len() <= MAX_VIEW_DEPTH);
    let children = &mut containers.last_mut().ok_or(())?.1;
    assert!(children.len() < MAX_VIEW_NODES);
    let before = children.len();
    children.push(node);
    assert_eq!(children.len(), before + 1);
    assert!(children.len() <= MAX_VIEW_NODES);
    Ok(())
}

fn push_view_node(
    containers: &mut [ViewContainer],
    root: &mut Option<ViewNode>,
    node: ViewNode,
) -> Result<(), ()> {
    assert!(containers.len() <= MAX_VIEW_DEPTH);
    if let Some(parent) = containers.last_mut() {
        assert!(parent.1.len() < MAX_VIEW_NODES);
        let before = parent.1.len();
        parent.1.push(node);
        assert_eq!(parent.1.len(), before + 1);
    } else if root.replace(node).is_some() {
        return Err(());
    }
    Ok(())
}

fn take<'a>(bytes: &'a [u8], position: &mut usize, length: usize) -> Result<&'a [u8], ()> {
    assert!(*position <= bytes.len());
    let before = *position;
    let end = position.checked_add(length).ok_or(())?;
    let value = bytes.get(*position..end).ok_or(())?;
    *position = end;
    assert_eq!(*position - before, length);
    assert_eq!(value.len(), length);
    assert!(*position <= bytes.len());
    Ok(value)
}

fn read_u32(bytes: &[u8], position: &mut usize) -> Result<u32, ()> {
    assert!(*position <= bytes.len());
    let before = *position;
    let value = u32::from_le_bytes(take(bytes, position, 4)?.try_into().map_err(|_| ())?);
    assert_eq!(*position - before, 4);
    assert!(*position <= bytes.len());
    Ok(value)
}

/// IDs form one event-target namespace across controls and nested menu actions.
/// Reusing an ID would make host input authorization ambiguous.
fn validate_view_ids(view: &ViewNode) -> Result<(), ()> {
    fn insert(ids: &mut BTreeSet<u32>, id: u32) -> Result<(), ()> {
        if id != 0 && ids.insert(id) { Ok(()) } else { Err(()) }
    }
    fn menu(items: &[MenuEntry], ids: &mut BTreeSet<u32>) -> Result<(), ()> {
        for item in items {
            match item {
                MenuEntry::Action { id, .. } => insert(ids, *id)?,
                MenuEntry::Submenu { items, .. } => menu(items, ids)?,
                MenuEntry::Separator => {}
            }
        }
        Ok(())
    }
    fn visit(view: &ViewNode, ids: &mut BTreeSet<u32>) -> Result<(), ()> {
        match view {
            ViewNode::Theme { child, .. } => visit(child, ids),
            ViewNode::Scroll { id, child, .. } => {
                insert(ids, *id)?;
                visit(child, ids)
            }
            ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
                for child in children {
                    visit(child, ids)?;
                }
                Ok(())
            }
            ViewNode::ContextMenu { child, items } => {
                menu(items, ids)?;
                visit(child, ids)
            }
            ViewNode::SegmentedControl { id, segments, .. } => {
                insert(ids, *id)?;
                for segment in segments {
                    insert(ids, segment.id)?;
                }
                Ok(())
            }
            ViewNode::PopupButton { id, items, .. } => {
                insert(ids, *id)?;
                menu(items, ids)
            }
            ViewNode::Canvas { id: 0, .. } => Ok(()),
            ViewNode::Button { id, .. }
            | ViewNode::Checkbox { id, .. }
            | ViewNode::Radio { id, .. }
            | ViewNode::ListRow { id, .. }
            | ViewNode::TextField { id, .. }
            | ViewNode::AutocompleteField { id, .. }
            | ViewNode::Canvas { id, .. }
            | ViewNode::JpegImage { id, .. } => insert(ids, *id),
            ViewNode::FileImage { .. } | ViewNode::Text { .. } => Ok(()),
        }
    }
    visit(view, &mut BTreeSet::new())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::runtime::syscall::Event;
    use playbit_api::ui::{
        AutocompleteItem, ButtonStyle, CanvasFrames, CanvasItem, CanvasRect, Color, MenuItem,
        TextStyle, Theme,
    };

    fn encoded_segments() -> Vec<u8> {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.segmented_control(
            1,
            &[
                playbit_api::ui::Segment { id: 2, label: "Slow", disabled: false },
                playbit_api::ui::Segment { id: 3, label: "Medium", disabled: true },
                playbit_api::ui::Segment { id: 4, label: "Fast", disabled: false },
            ],
            3,
            false,
        );
        encoder.column_end();
        encoder.finish()
    }
    #[test]
    fn scroll_admits_nested_controls_and_rejects_bad_flags_and_ids() {
        // Scrolls preserve child input authority and reject malformed records atomically.
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.scroll_start(1, true);
        encoder.button(2, "Nested", ButtonStyle::Default, false);
        encoder.scroll_end();
        encoder.column_end();
        let encoded = encoder.finish();
        let view = decode_view(&encoded).unwrap();
        assert!(
            crate::runtime::syscall::validate_input(
                &view,
                &Event {
                    kind: playbit_api::abi::syscall::EventKind::Click,
                    target: 2,
                    data: vec![]
                }
            )
            .is_ok()
        );
        assert!(
            crate::runtime::syscall::validate_input(
                &view,
                &Event {
                    kind: playbit_api::abi::syscall::EventKind::Click,
                    target: 1,
                    data: vec![]
                }
            )
            .is_err()
        );
        let mut invalid = encoded.clone();
        invalid[14..18].copy_from_slice(&0u32.to_le_bytes());
        assert!(decode_view(&invalid).is_err());
        let mut invalid = encoded.clone();
        invalid[22] = 2;
        assert!(decode_view(&invalid).is_err());
        let mut invalid = encoded.clone();
        invalid[14..18].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode_view(&invalid).is_err());
        for end in 0..encoded.len() {
            assert!(decode_view(&encoded[..end]).is_err());
        }
    }
    #[test]
    fn embedded_image_admission_bounds_bytes_labels_and_identity() {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.jpeg_image(1, 1024, 681, b"encoded photo", "Photo");
        encoder.column_end();
        let encoded = encoder.finish();
        let view = decode_view(&encoded).unwrap();
        assert!(
            crate::runtime::syscall::validate_input(
                &view,
                &Event {
                    kind: playbit_api::abi::syscall::EventKind::Click,
                    target: 1,
                    data: vec![]
                }
            )
            .is_err()
        );
        let mut invalid = encoded.clone();
        invalid[30..34].copy_from_slice(&4097u32.to_le_bytes());
        assert!(decode_view(&invalid).is_err());
        for end in 0..encoded.len() {
            assert!(decode_view(&encoded[..end]).is_err());
        }
    }
    #[test]
    fn segmented_group_roundtrip_and_input_authorization() {
        use crate::runtime::syscall::validate_input;
        use playbit_api::abi::syscall::EventKind;
        let view = decode_view(&encoded_segments()).unwrap();
        for target in [1, 2, 3, 4, 5] {
            let event = Event { kind: EventKind::Click, target, data: Vec::new() };
            assert_eq!(validate_input(&view, &event).is_ok(), target == 2 || target == 4);
        }
        let ViewNode::Theme { child, .. } = view else { panic!() };
        let ViewNode::Column(mut children) = *child else { panic!() };
        let ViewNode::SegmentedControl { disabled, .. } = &mut children[0] else { panic!() };
        *disabled = true;
        assert!(
            validate_input(
                &children[0],
                &Event { kind: EventKind::Click, target: 2, data: Vec::new() }
            )
            .is_err()
        );
    }
    #[test]
    fn segmented_decoder_rejects_malformed_records() {
        let bytes = encoded_segments();
        // magic + column header + segmented header = payload offset 22.
        for (offset, value) in
            [(22, 2), (27, 2), (27, 6), (23, 99), (28, 0), (28, 1), (41, 2), (32, 2), (37, 255)]
        {
            let mut invalid = bytes.clone();
            invalid[offset] = value;
            assert!(decode_view(&invalid).is_err(), "offset {offset}, value {value}");
        }
        for length in 0..bytes.len() {
            assert!(decode_view(&bytes[..length]).is_err());
        }
    }
    #[test]
    fn segment_ids_share_the_global_event_namespace() {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.segmented_control(
            1,
            &[
                playbit_api::ui::Segment { id: 2, label: "A", disabled: false },
                playbit_api::ui::Segment { id: 3, label: "B", disabled: false },
                playbit_api::ui::Segment { id: 4, label: "C", disabled: false },
            ],
            2,
            false,
        );
        encoder.button(3, "Collision", ButtonStyle::Default, false);
        encoder.column_end();
        assert!(decode_view(&encoder.finish()).is_err());
    }

    #[test]
    fn static_canvases_have_no_guest_input_target() {
        let rect = CanvasRect { x: 0, y: 0, width: 16, height: 16, color: Color(0xff0000ff) };
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.canvas_static(16, 16, &[rect]);
        encoder.canvas_static(16, 16, &[rect]);
        encoder.canvas(7, 16, 16, CanvasFrames::Continuous, &[CanvasItem::Rect(rect)]);
        encoder.column_end();
        let view = decode_view(&encoder.finish()).unwrap();
        let ViewNode::Theme { child, .. } = &view else { panic!("expected theme") };
        let ViewNode::Column(nodes) = child.as_ref() else { panic!("expected column") };
        assert!(matches!(nodes[0], ViewNode::Canvas { id: 0, .. }));
        assert!(matches!(nodes[1], ViewNode::Canvas { id: 0, .. }));
        assert!(matches!(nodes[2], ViewNode::Canvas { id: 7, .. }));
        let event = Event {
            kind: playbit_api::abi::syscall::EventKind::Frame,
            target: 0,
            data: 1u32.to_le_bytes().to_vec(),
        };
        assert!(crate::runtime::syscall::validate_input(&view, &event).is_err());
        assert!(
            crate::runtime::syscall::validate_input(&view, &Event { target: 7, ..event },).is_ok()
        );
    }

    #[test]
    fn root_canvas_extent_clear_and_frame_mode_round_trip() {
        for (extent, clear_color, frames) in [
            (CanvasExtent::Viewport, None, CanvasFrames::Static),
            (CanvasExtent::Viewport, None, CanvasFrames::Continuous),
            (CanvasExtent::Viewport, Some(Color(0)), CanvasFrames::Continuous),
            (CanvasExtent::Window, Some(Color(0x923456ff)), CanvasFrames::Static),
            (CanvasExtent::Window, None, CanvasFrames::Continuous),
        ] {
            let mut state = playbit_api::context::ContextState::default();
            let mut encoder = state.view();
            encoder.canvas_with_options(
                7,
                16,
                16,
                CanvasOptions { frames, extent, clear_color },
                &[],
            );
            let bytes = encoder.finish();
            if extent == CanvasExtent::Viewport && clear_color.is_none() {
                let mut viewport = state.view();
                viewport.viewport_canvas(7, 16, 16, frames, &[]);
                assert_eq!(viewport.finish(), bytes);
            }
            let view = decode_view(&bytes).unwrap();
            let ViewNode::Theme { child, .. } = &view else { panic!("theme") };
            assert_eq!(
                **child,
                ViewNode::Canvas {
                    id: 7,
                    width: 16,
                    height: 16,
                    extent,
                    clear_color,
                    frames,
                    items: vec![],
                }
            );
            let event = Event {
                kind: playbit_api::abi::syscall::EventKind::Frame,
                target: 7,
                data: 1u32.to_le_bytes().to_vec(),
            };
            assert_eq!(
                crate::runtime::syscall::validate_input(&view, &event).is_ok(),
                frames == CanvasFrames::Continuous,
            );

            let mut nested = bytes[..4].to_vec();
            nested.extend_from_slice(&[VIEW_COLUMN_START, 0, 0, 0, 0, 0, 0, 0, 0]);
            nested.extend_from_slice(&bytes[4..]);
            nested.extend_from_slice(&[VIEW_COLUMN_END, 0, 0, 0, 0, 0, 0, 0, 0]);
            assert!(decode_view(&nested).is_err());
            let mut both_extents = bytes.clone();
            // PBV1 + command header + width/height.
            both_extents[21] |=
                (playbit_api::ui::CANVAS_FLAG_WINDOW | playbit_api::ui::CANVAS_FLAG_VIEWPORT) as u8;
            assert!(decode_view(&both_extents).is_err());
            let mut wrong_clear_length = bytes;
            wrong_clear_length[21] ^= playbit_api::ui::CANVAS_FLAG_CLEAR as u8;
            assert!(decode_view(&wrong_clear_length).is_err());
        }
    }

    #[test]
    fn window_canvas_accepts_large_logical_geometry_and_checks_pointer_bounds() {
        use playbit_api::abi::syscall::{EventKind, PointerInput};
        let (width, height) = (5500, 3000);
        let edge = CanvasRect {
            x: width - 1,
            y: height - 1,
            width: 1,
            height: 1,
            color: Color(0x336699ff),
        };
        let mut state = playbit_api::context::ContextState::default();
        let mut encoder = state.view();
        encoder.canvas_with_options(
            7,
            width,
            height,
            CanvasOptions { extent: CanvasExtent::Window, ..Default::default() },
            &[CanvasItem::Rect(edge)],
        );
        let bytes = encoder.finish();
        let scene = decode_view(&bytes).unwrap();
        let ViewNode::Theme { child, .. } = &scene else { panic!("theme") };
        let ViewNode::Canvas { items, .. } = child.as_ref() else { panic!("canvas") };
        assert_eq!(items, &[super::CanvasItem::Rect(edge)]);
        for (x, y, accepted) in
            [(width, height, true), (width + 1, height, false), (width, height + 1, false)]
        {
            let data = PointerInput { x, y, phase: 1, buttons: 1 }.encode().to_vec();
            let event = Event { kind: EventKind::Pointer, target: 7, data };
            assert_eq!(crate::runtime::syscall::validate_input(&scene, &event).is_ok(), accepted);
        }
        // Width follows the command header. Verify the exact logical bound.
        let mut widest = bytes.clone();
        for (edge, accepted) in [
            (playbit_api::ui::MAX_WINDOW_CANVAS_EDGE, true),
            (playbit_api::ui::MAX_WINDOW_CANVAS_EDGE + 1, false),
        ] {
            widest[13..17].copy_from_slice(&edge.to_le_bytes());
            assert_eq!(decode_view(&widest).is_ok(), accepted);
        }
        let mut viewport = bytes;
        viewport[21] = playbit_api::ui::CANVAS_FLAG_VIEWPORT as u8;
        assert!(decode_view(&viewport).is_err());
    }

    #[test]
    fn window_style_is_explicit_validated_scene_metadata() {
        let style = WindowStyle::STANDARD
            | WindowStyle::HIDDEN_TITLE
            | WindowStyle::THEME_DARK
            | WindowStyle::HIDE_SESSION_BUTTON;
        let mut state = playbit_api::context::ContextState::default();
        let mut encoder = state.view();
        encoder.window_style(style);
        encoder.theme(&[]);
        encoder.column_start();
        encoder.column_end();
        let bytes = encoder.finish();
        assert!(
            matches!(decode_view(&bytes), Ok(ViewNode::Theme {window_style: Some(actual), ..}) if actual == style)
        );
        // A style command has a 9-byte header and four little-endian flag bytes.
        let mut duplicate = bytes.clone();
        duplicate.splice(17..17, bytes[4..17].iter().copied());
        assert!(decode_view(&duplicate).is_err());
        let mut after_root = bytes.clone();
        after_root[4..].rotate_left(13);
        assert!(decode_view(&after_root).is_err());
        for flags in [WindowStyle::THEME_BRIGHT.bits() | WindowStyle::THEME_DARK.bits(), 1 << 10] {
            let mut invalid = bytes.clone();
            invalid[13..17].copy_from_slice(&flags.to_le_bytes());
            assert!(decode_view(&invalid).is_err());
        }
        let mut encoder = state.view();
        encoder.theme(&[]);
        encoder.window_style(WindowStyle::NONE);
        encoder.column_start();
        encoder.column_end();
        assert!(matches!(
            decode_view(&encoder.finish()),
            Ok(ViewNode::Theme { window_style: Some(WindowStyle::NONE), .. })
        ));
    }

    #[test]
    fn window_size_constraints_validate_the_whole_scene_request() {
        // Round-trip fractional dp at a fixed aspect and reject malformed size and ratio words.
        let fixed = WindowSizeConstraints {
            min_width: 640.5,
            min_height: 360.25,
            max_width: 640.5,
            max_height: 360.25,
            aspect_ratio: 640.5 / 360.25,
        };
        let mut state = playbit_api::context::ContextState::default();
        let mut encoder = state.view();
        encoder.window_style(WindowStyle::STANDARD);
        encoder.window_size_constraints(fixed);
        encoder.column_start();
        encoder.column_end();
        let bytes = encoder.finish();
        assert!(matches!(decode_view(&bytes), Ok(ViewNode::Theme {
            window_size_constraints: Some(actual), .. }) if actual == fixed));
        let start = 4 + 9 + 4;
        let payload = start + 9;
        let root = payload + 20;
        for (offset, word) in [
            (start + 1, 1),
            (start + 5, 19),
            (start + 5, 21),
            (payload, 641f32.to_bits()),
            (payload, f32::NAN.to_bits()),
            (payload + 4, 361f32.to_bits()),
            (payload + 4, (-0.5f32).to_bits()),
            (payload + 8, (playbit_api::ui::MAX_WINDOW_CANVAS_EDGE as f32).next_up().to_bits()),
            (payload + 12, f32::INFINITY.to_bits()),
            (payload + 16, 1f32.to_bits()),
            (payload + 16, f32::INFINITY.to_bits()),
            (payload + 16, f32::NAN.to_bits()),
            (payload + 16, (-1f32).to_bits()),
        ] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
            assert!(decode_view(&invalid).is_err());
        }
        let mut duplicate = bytes.clone();
        duplicate.splice(root..root, bytes[start..root].iter().copied());
        assert!(decode_view(&duplicate).is_err());
        let mut inside_root = bytes.clone();
        inside_root[start..root + 9].rotate_left(root - start);
        assert!(decode_view(&inside_root).is_err());
        let mut encoder = state.view();
        encoder.column_start();
        encoder.column_end();
        assert!(matches!(
            decode_view(&encoder.finish()),
            Ok(ViewNode::Theme { window_size_constraints: None, .. })
        ));
    }

    #[test]
    fn packed_canvas_pixels_round_trip_and_reject_malformed_spans() {
        // Compare SDK/host wire interpretation and fail before retaining malformed pixels.
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 0];
        let bounds = CanvasRect { x: 2, y: 3, width: 12, height: 8, color: Color(0) };
        let mut state = playbit_api::context::ContextState::default();
        let mut encoder = state.view();
        encoder.column_start();
        encoder.canvas(
            7,
            16,
            16,
            CanvasFrames::Static,
            &[CanvasItem::Pixels(playbit_api::ui::CanvasPixels {
                bounds,
                source_width: 2,
                source_height: 2,
                rgba: &rgba,
            })],
        );
        encoder.column_end();
        let bytes = encoder.finish();
        let ViewNode::Theme { child, .. } = decode_view(&bytes).unwrap() else { panic!("theme") };
        let ViewNode::Column(children) = *child else { panic!("column") };
        let ViewNode::Canvas { items, .. } = &children[0] else { panic!("canvas") };
        assert_eq!(
            items,
            &[super::CanvasItem::Pixels {
                bounds,
                source_width: 2,
                source_height: 2,
                rgba: rgba.into(),
            }]
        );
        // Canvas payload starts at byte 22; the pixel payload starts at byte 74.
        assert_eq!(bytes[13], VIEW_CANVAS);
        for (offset, value) in [(38, 3u32), (58, 1), (62, 0), (66, 4097), (70, 15), (70, 17)] {
            let mut bad = bytes.clone();
            bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(decode_view(&bad).is_err(), "offset {offset}, value {value}");
        }
        let mut truncated = bytes.clone();
        truncated.remove(74);
        assert!(decode_view(&truncated).is_err());
    }

    #[test]
    fn packed_canvas_pixel_budget_counts_every_canvas() {
        // Each command fits independently, but their aggregate storage must fit one scene.
        let rgba = vec![0; playbit_api::ui::MAX_CANVAS_PIXEL_BYTES];
        let bounds = CanvasRect { x: 0, y: 0, width: 4096, height: 2048, color: Color(0) };
        let mut state = playbit_api::context::ContextState::default();
        let mut encoder = state.view();
        encoder.column_start();
        encoder.canvas(
            7,
            4096,
            2048,
            CanvasFrames::Static,
            &[CanvasItem::Pixels(playbit_api::ui::CanvasPixels {
                bounds,
                source_width: 4096,
                source_height: 2048,
                rgba: &rgba,
            })],
        );
        encoder.column_end();
        let mut bytes = encoder.finish();
        assert!(decode_view(&bytes).is_ok());
        // Construct another valid canvas independently to bypass SDK aggregate
        // admission and exercise the decoder's exact-limit-plus-one-frame check.
        let mut other_state = playbit_api::context::ContextState::default();
        let mut other = other_state.view();
        other.column_start();
        other.canvas(
            8,
            1,
            1,
            CanvasFrames::Static,
            &[CanvasItem::Pixels(playbit_api::ui::CanvasPixels {
                bounds: CanvasRect { x: 0, y: 0, width: 1, height: 1, color: Color(0) },
                source_width: 1,
                source_height: 1,
                rgba: &[0; 4],
            })],
        );
        other.column_end();
        let second = other.finish();
        let end = bytes.split_off(bytes.len() - 9);
        bytes.extend_from_slice(&second[13..second.len() - 9]);
        bytes.extend_from_slice(&end);
        let excessive = bytes;
        assert!(excessive.len() < playbit_api::abi::syscall::MAX_CALL_BYTES);
        assert!(decode_view(&excessive).is_err());
    }

    #[test]
    fn read_only_field_and_large_label_decode_as_semantics() {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.text(TextStyle::LabelLarge, "Large");
        encoder.text_field(7, &mut "Copy me".into(), "", false, false, true);
        encoder.column_end();
        let view = decode_view(&encoder.finish()).unwrap();
        let ViewNode::Theme { child, .. } = view else { panic!("expected theme") };
        let ViewNode::Column(nodes) = *child else { panic!("expected column") };
        assert!(matches!(&nodes[0], ViewNode::Text { style: TextStyle::LabelLarge, .. }));
        assert!(matches!(
            &nodes[1],
            ViewNode::TextField { id: 7, read_only: true, disabled: false, .. }
        ));
    }

    #[test]
    fn menu_icons_and_checkability_round_trip_and_reject_invalid_bytes() {
        // Exercise every admitted icon and check state through the real scene codec.
        for icon in [
            None,
            Some(MenuIcon::Avatar),
            Some(MenuIcon::Signature),
            Some(MenuIcon::Copy),
            Some(MenuIcon::ExitRight),
            Some(MenuIcon::OpenBook),
            Some(MenuIcon::Talk),
        ] {
            for checked in [None, Some(false), Some(true)] {
                let mut encoder_state = playbit_api::context::ContextState::default();
                let mut encoder = encoder_state.view();
                encoder.column_start();
                encoder.popup_button(
                    1,
                    "Menu",
                    &[MenuItem::Submenu {
                        label: "More",
                        icon,
                        disabled: false,
                        items: &[MenuItem::Action {
                            id: 2,
                            label: "Action",
                            icon,
                            checked,
                            disabled: true,
                            destructive: true,
                        }],
                    }],
                    false,
                );
                encoder.column_end();
                let bytes = encoder.finish();
                let ViewNode::Theme { child, .. } = decode_view(&bytes).unwrap() else { panic!() };
                let ViewNode::Column(mut children) = *child else { panic!() };
                let ViewNode::PopupButton { items, .. } = children.remove(0) else { panic!() };
                assert_eq!(
                    items,
                    vec![MenuEntry::Submenu {
                        label: "More".into(),
                        icon: icon.map(Into::into),
                        disabled: false,
                        items: vec![MenuEntry::Action {
                            id: 2,
                            label: "Action".into(),
                            icon: icon.map(Into::into),
                            checked,
                            disabled: true,
                            destructive: true,
                        }],
                    }]
                );
                for length in 0..bytes.len() {
                    assert!(decode_view(&bytes[..length]).is_err());
                }
                // Corrupt the icon/flags immediately preceding the action label length.
                let label_offset = bytes.windows(6).position(|bytes| bytes == b"Action").unwrap();
                let icon_offset = label_offset - 5;
                let mut invalid = bytes.clone();
                invalid[icon_offset] = 255;
                assert!(decode_view(&invalid).is_err());
                invalid = bytes;
                invalid[icon_offset - 1] = MENU_CHECKED;
                assert!(decode_view(&invalid).is_err(), "checked requires checkability");
            }
        }
    }

    #[test]
    fn view_stream_decodes_to_a_host_owned_tree() {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.text(TextStyle::Title, "Recipes");
        encoder.text_field(9, &mut "Soup".into(), "Recipe name", false, false, false);
        encoder.autocomplete_field(
            12,
            "Ca",
            "Animal",
            &[AutocompleteItem { label: "Cats", value: "cat" }],
            false,
        );
        encoder.button(7, "Add", ButtonStyle::Primary, false);
        encoder.popup_button(
            10,
            "Animals",
            &[MenuItem::Action {
                icon: None,
                id: 11,
                label: "Cats",
                checked: Some(true),
                disabled: false,
                destructive: false,
            }],
            false,
        );
        encoder.context_menu_start(&[MenuItem::Action {
            icon: None,
            id: 13,
            label: "Inspect",
            checked: None,
            disabled: false,
            destructive: false,
        }]);
        encoder.button(14, "Right-click", ButtonStyle::Default, false);
        encoder.context_menu_end();
        encoder.column_end();
        let view = decode_view(&encoder.finish()).unwrap();

        assert_eq!(
            view,
            ViewNode::Theme {
                theme: Theme::light(),
                window_background_specified: false,
                window_style: None,
                window_size_constraints: None,
                child: Box::new(ViewNode::Column(vec![
                    ViewNode::Text { style: TextStyle::Title, text: "Recipes".to_owned() },
                    ViewNode::TextField {
                        id: 9,
                        value: "Soup".to_owned(),
                        placeholder: "Recipe name".to_owned(),
                        multiline: false,
                        disabled: false,
                        read_only: false,
                    },
                    ViewNode::AutocompleteField {
                        id: 12,
                        value: "Ca".to_owned(),
                        placeholder: "Animal".to_owned(),
                        suggestions: vec![AutocompleteEntry {
                            label: "Cats".to_owned(),
                            value: "cat".to_owned(),
                        }],
                        disabled: false,
                    },
                    ViewNode::Button {
                        id: 7,
                        label: "Add".to_owned(),
                        style: ButtonStyle::Primary,
                        disabled: false,
                    },
                    ViewNode::PopupButton {
                        id: 10,
                        label: "Animals".to_owned(),
                        items: vec![MenuEntry::Action {
                            icon: None,
                            id: 11,
                            label: "Cats".to_owned(),
                            checked: Some(true),
                            disabled: false,
                            destructive: false,
                        }],
                        disabled: false,
                    },
                    ViewNode::ContextMenu {
                        items: vec![MenuEntry::Action {
                            icon: None,
                            id: 13,
                            label: "Inspect".to_owned(),
                            checked: None,
                            disabled: false,
                            destructive: false,
                        }],
                        child: Box::new(ViewNode::Button {
                            id: 14,
                            label: "Right-click".to_owned(),
                            style: ButtonStyle::Default,
                            disabled: false,
                        }),
                    },
                ])),
            }
        );
    }

    #[test]
    fn theme_override_round_trips_and_rejects_duplicates() {
        use playbit_api::ui::{Color, ThemeOverride};
        let override_ = ThemeOverride { role: ThemeColor::Shadow, color: Color(0x123456ff) };
        for count in [1, 2] {
            let mut view_state = playbit_api::context::ContextState::default();
            let mut view = view_state.view();
            view.theme(&vec![override_; count]);
            view.column_start();
            view.button(1, "Delete", ButtonStyle::Destructive, false);
            view.column_end();
            let decoded = decode_view(&view.finish());
            if count == 2 {
                assert!(decoded.is_err(), "duplicate semantic roles must remain invalid");
            } else {
                let ViewNode::Theme { theme, .. } = decoded.unwrap() else { panic!() };
                assert_eq!(theme.color(override_.role), override_.color);
            }
        }
        assert!(ThemeColor::from_wire(ThemeColor::COUNT as u8).is_none());
    }

    #[test]
    fn image_description_annotation_is_adjacent_bounded_and_unambiguous() {
        fn encoded(label: &str) -> Vec<u8> {
            let mut view_state = playbit_api::context::ContextState::default();
            let mut view = view_state.view();
            view.column_start();
            view.file_image(
                FileReference {
                    volume: playbit_api::abi::syscall::VolumeHandle { generation: 1, slot: 1 },
                    entry_id: [1; 15],
                },
                10,
                20,
                label,
            );
            view.column_end();
            view.finish()
        }
        fn label(bytes: &[u8]) -> Option<String> {
            let ViewNode::Theme { child, .. } = decode_view(bytes).unwrap() else { panic!() };
            let ViewNode::Column(children) = *child else { panic!() };
            let ViewNode::FileImage { label, .. } = &children[0] else { panic!() };
            label.clone()
        }
        for text in ["", "海辺 🦋", &"🦋".repeat(1024)] {
            assert_eq!(label(&encoded(text)).as_deref(), Some(text));
        }
        let bytes = encoded("x");
        let annotation = &bytes[53..63];
        let mut primitive = bytes.clone();
        primitive.drain(53..63);
        assert_eq!(label(&primitive), None, "image20 is still the current primitive");
        let mut bad_id = bytes.clone();
        bad_id[54] = 1;
        let mut bad_utf8 = bytes.clone();
        bad_utf8[62] = 0xff;
        let mut duplicate = bytes.clone();
        duplicate.splice(63..63, annotation.iter().copied());
        let mut orphan = bytes.clone();
        orphan.drain(13..53);
        let mut misplaced = bytes.clone();
        // A valid text command between the image and annotation breaks adjacency.
        misplaced.splice(53..53, [VIEW_TEXT, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        let mut after_end = primitive.clone();
        after_end.extend_from_slice(annotation);
        let mut oversized = encoded(&"x".repeat(4096));
        oversized[58..62].copy_from_slice(&4097_u32.to_le_bytes());
        oversized.insert(62, b'x');
        for malformed in [bad_id, bad_utf8, duplicate, orphan, misplaced, after_end, oversized] {
            assert!(decode_view(&malformed).is_err());
        }
    }
    #[test]
    #[ignore = "reference-machine scene decode timing; run optimized and without competing builds"]
    fn benchmark_scene_decode() {
        for rows in [1, 32, 64] {
            let mut encoder_state = playbit_api::context::ContextState::default();
            let mut encoder = encoder_state.view();
            encoder.column_start();
            for _ in 0..rows {
                encoder.text(TextStyle::Body, "Representative local TODO row");
            }
            encoder.column_end();
            let bytes = encoder.finish();
            let mut samples = Vec::with_capacity(1000);
            for n in 0..1100 {
                let start = std::time::Instant::now();
                // Four active scenes, including allocation, ID validation and teardown.
                for _ in 0..4 {
                    std::hint::black_box(decode_view(std::hint::black_box(&bytes)).unwrap());
                }
                let elapsed = start.elapsed().as_nanos();
                if n >= 100 {
                    samples.push(elapsed);
                }
            }
            samples.sort_unstable();
            println!(
                "scene decode, four sessions x {rows} rows, 1000 samples, ns p50/p99/max: {}/{}/{}",
                samples[499], samples[989], samples[999]
            );
            // Decode is part of the existing <2 ms engine/guest admission allocation.
            assert!(samples[989] < 2_000_000);
        }
    }
    #[test]
    fn scene_wire_record_and_depth_boundaries_return_errors_without_panics() {
        fn record(bytes: &mut Vec<u8>, kind: u8, payload: &[u8]) {
            bytes.push(kind);
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
        }
        for rows in [MAX_VIEW_NODES - 2, MAX_VIEW_NODES - 1] {
            let mut bytes = VIEW_MAGIC.to_vec();
            record(&mut bytes, VIEW_COLUMN_START, &[]);
            for _ in 0..rows {
                record(&mut bytes, VIEW_TEXT, &[TextStyle::Body as u8]);
            }
            record(&mut bytes, VIEW_COLUMN_END, &[]);
            assert_eq!(decode_view(&bytes).is_ok(), rows == MAX_VIEW_NODES - 2);
        }
        for depth in [MAX_VIEW_DEPTH, MAX_VIEW_DEPTH + 1] {
            let mut bytes = VIEW_MAGIC.to_vec();
            for _ in 0..depth {
                record(&mut bytes, VIEW_COLUMN_START, &[]);
            }
            record(&mut bytes, VIEW_TEXT, &[TextStyle::Body as u8]);
            for _ in 0..depth {
                record(&mut bytes, VIEW_COLUMN_END, &[]);
            }
            assert_eq!(decode_view(&bytes).is_ok(), depth == MAX_VIEW_DEPTH);
            for end in 0..bytes.len() {
                assert!(decode_view(&bytes[..end]).is_err());
            }
        }
        let oversized = vec![0; playbit_api::abi::syscall::MAX_CALL_BYTES + 1];
        assert!(decode_view(&oversized).is_err());
    }
}
