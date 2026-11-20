//! Real guest artifacts compiled and embedded for native regression tests.

#[cfg(feature = "wasm")]
pub enum Guest {
    Todo,
    TodoRoot,
    SharedNote,
    UiGallery,
    LittleList,
}

#[cfg(feature = "wasm")]
impl Guest {
    pub fn wasm(self) -> &'static [u8] {
        match self {
            Self::Todo => include_bytes!(concat!(
                env!("OUT_DIR"),
                "/target/wasm32-unknown-unknown/release/todo.wasm"
            )),
            Self::TodoRoot => include_bytes!(concat!(
                env!("OUT_DIR"),
                "/root-target/wasm32-unknown-unknown/release/todo.wasm"
            )),
            Self::SharedNote => include_bytes!(concat!(
                env!("OUT_DIR"),
                "/target/wasm32-unknown-unknown/release/shared_note.wasm"
            )),
            Self::UiGallery => include_bytes!(concat!(
                env!("OUT_DIR"),
                "/target/wasm32-unknown-unknown/release/ui_gallery.wasm"
            )),
            Self::LittleList => include_bytes!(concat!(
                env!("OUT_DIR"),
                "/target/wasm32-unknown-unknown/release/little_list.wasm"
            )),
        }
    }
}
