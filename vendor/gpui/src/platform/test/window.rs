use crate::{
    AnyWindowHandle, AtlasKey, AtlasTextureId, AtlasTile, Bounds, DispatchEventResult, GpuSpecs,
    Pixels, PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow,
    Point, PromptButton, RequestFrameOptions, Size, TestPlatform, TileId, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams,
};
use collections::HashMap;
use parking_lot::Mutex;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::{
    rc::{Rc, Weak},
    sync::{self, Arc},
};

pub(crate) struct TestWindowState {
    #[cfg(target_os = "macos")]
    pub(crate) frame_needed: bool,
    pub(crate) bounds: Bounds<Pixels>,
    scale_factor: f32,
    pub(crate) handle: AnyWindowHandle,
    display: Rc<dyn PlatformDisplay>,
    pub(crate) title: Option<String>,
    #[cfg(target_os = "macos")]
    titlebar_button: Option<crate::MacTitlebarButton>,
    #[cfg(target_os = "macos")]
    titlebar_action: Option<Box<dyn FnMut(&'static str)>>,
    pub(crate) edited: bool,
    platform: Weak<TestPlatform>,
    sprite_atlas: Arc<dyn PlatformAtlas>,
    pub(crate) should_close_handler: Option<Box<dyn FnMut() -> bool>>,
    #[cfg(target_os = "macos")]
    non_draggable_regions: Vec<Bounds<Pixels>>,
    hit_test_window_control_callback:
        Option<Box<dyn FnMut(Option<Point<Pixels>>) -> Option<WindowControlArea>>>,
    #[cfg(target_os = "macos")]
    accessibility_callback: Option<Box<dyn FnMut(accesskit::ActionRequest)>>,
    #[cfg(target_os = "macos")]
    accessibility_nodes: Vec<crate::AccessibilityNode>,
    input_callback: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    request_frame_callback: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    active_status_change_callback: Option<Box<dyn FnMut(bool)>>,
    hover_status_change_callback: Option<Box<dyn FnMut(bool)>>,
    resize_callback: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved_callback: Option<Box<dyn FnMut()>>,
    input_handler: Option<PlatformInputHandler>,
    is_fullscreen: bool,
}

#[derive(Clone)]
pub(crate) struct TestWindow(pub(crate) Rc<Mutex<TestWindowState>>);

impl HasWindowHandle for TestWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::Unavailable)
    }
}

impl HasDisplayHandle for TestWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        unimplemented!("Test Windows are not backed by a real platform window")
    }
}

impl TestWindow {
    #[cfg(target_os = "macos")]
    pub(crate) fn click_titlebar_button(&self) -> bool {
        let mut state = self.0.lock();
        let Some(spec) = state.titlebar_button.filter(|button| button.enabled) else {
            return false;
        };
        let mut dispatch =
            state.titlebar_action.take().expect("window registered an action handler");
        drop(state);
        dispatch(spec.action);
        self.0.lock().titlebar_action = Some(dispatch);
        true
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn window_can_drag_at(&self, position: Point<Pixels>) -> bool {
        !self.0.lock().non_draggable_regions.iter().any(|rect| rect.contains(&position))
    }

    #[allow(
        dead_code,
        reason = "Test window hook is retained when Playbit enables gpui test support without upstream tests."
    )]
    pub(crate) fn simulate_frame(&self, options: RequestFrameOptions) {
        let callback = self.0.lock().request_frame_callback.take();
        if let Some(mut callback) = callback {
            callback(options);
            self.0.lock().request_frame_callback = Some(callback);
        }
    }

    pub fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        platform: Weak<TestPlatform>,
        display: Rc<dyn PlatformDisplay>,
    ) -> Self {
        Self(Rc::new(Mutex::new(TestWindowState {
            scale_factor: 2.0,
            #[cfg(target_os = "macos")]
            frame_needed: false,
            bounds: params.bounds,
            display,
            platform,
            handle,
            sprite_atlas: Arc::new(TestAtlas::new()),
            title: Default::default(),
            #[cfg(target_os = "macos")]
            titlebar_button: None,
            #[cfg(target_os = "macos")]
            titlebar_action: None,
            edited: false,
            should_close_handler: None,
            #[cfg(target_os = "macos")]
            non_draggable_regions: Vec::new(),
            hit_test_window_control_callback: None,
            input_callback: None,
            request_frame_callback: None,
            #[cfg(target_os = "macos")]
            accessibility_callback: None,
            #[cfg(target_os = "macos")]
            accessibility_nodes: Vec::new(),
            active_status_change_callback: None,
            hover_status_change_callback: None,
            resize_callback: None,
            moved_callback: None,
            input_handler: None,
            is_fullscreen: false,
        })))
    }

    pub fn simulate_scale_factor(&mut self, scale: f32) {
        assert!(scale.is_finite() && scale > 0.);
        self.0.lock().scale_factor = scale;
        self.simulate_resize(self.bounds().size);
    }

    pub fn simulate_resize(&mut self, size: Size<Pixels>) {
        let scale_factor = self.scale_factor();
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.resize_callback.take() else {
            return;
        };
        lock.bounds.size = size;
        drop(lock);
        callback(size, scale_factor);
        self.0.lock().resize_callback = Some(callback);
    }

    pub(crate) fn simulate_active_status_change(&self, active: bool) {
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.active_status_change_callback.take() else {
            return;
        };
        drop(lock);
        callback(active);
        self.0.lock().active_status_change_callback = Some(callback);
    }

    pub fn simulate_input(&mut self, event: PlatformInput) -> bool {
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.input_callback.take() else {
            return false;
        };
        drop(lock);
        let result = callback(event);
        self.0.lock().input_callback = Some(callback);
        !result.propagate
    }
}

impl PlatformWindow for TestWindow {
    #[cfg(target_os = "macos")]
    fn set_window_non_draggable_regions(&self, regions: &[Bounds<Pixels>]) {
        self.0.lock().non_draggable_regions = regions.to_vec();
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.0.lock().bounds
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    #[cfg(target_os = "macos")]
    fn set_min_size(&mut self, _size: Size<Pixels>) {}

    fn resize(&mut self, size: Size<Pixels>) {
        let mut lock = self.0.lock();
        lock.bounds.size = size;
    }

    fn scale_factor(&self) -> f32 {
        self.0.lock().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }

    fn display(&self) -> Option<std::rc::Rc<dyn crate::PlatformDisplay>> {
        Some(self.0.lock().display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> crate::Modifiers {
        crate::Modifiers::default()
    }

    fn capslock(&self) -> crate::Capslock {
        crate::Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.lock().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.lock().input_handler.take()
    }

    fn prompt(
        &self,
        _level: crate::PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        Some(
            self.0
                .lock()
                .platform
                .upgrade()
                .expect("platform dropped")
                .prompt(msg, detail, answers),
        )
    }

    fn activate(&self) {
        self.0.lock().platform.upgrade().unwrap().set_active_window(Some(self.clone()))
    }

    fn is_active(&self) -> bool {
        false
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn set_title(&mut self, title: &str) {
        self.0.lock().title = Some(title.to_owned());
    }

    fn set_app_id(&mut self, _app_id: &str) {}

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    #[cfg(target_os = "macos")]
    fn set_macos_window_style(&self, style: crate::MacWindowStyle) {
        self.0.lock().titlebar_button = style.titlebar_button;
    }

    #[cfg(target_os = "macos")]
    fn on_titlebar_action(&self, callback: Box<dyn FnMut(&'static str)>) {
        self.0.lock().titlebar_action = Some(callback);
    }

    #[cfg(target_os = "macos")]
    fn titlebar_height(&self) -> Pixels {
        crate::px(28.)
    }

    #[cfg(target_os = "macos")]
    fn traffic_light_right(&self) -> Pixels {
        crate::px(69.)
    }

    fn set_edited(&mut self, edited: bool) {
        self.0.lock().edited = edited;
    }

    fn show_character_palette(&self) {
        unimplemented!()
    }

    fn minimize(&self) {
        unimplemented!()
    }

    fn zoom(&self) {
        unimplemented!()
    }

    fn toggle_fullscreen(&self) {
        let mut lock = self.0.lock();
        lock.is_fullscreen = !lock.is_fullscreen;
    }

    fn is_fullscreen(&self) -> bool {
        self.0.lock().is_fullscreen
    }

    #[cfg(target_os = "macos")]
    fn set_frame_demand(&self, needed: bool) {
        self.0.lock().frame_needed = needed;
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.lock().request_frame_callback = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(crate::PlatformInput) -> DispatchEventResult>) {
        self.0.lock().input_callback = Some(callback)
    }

    #[cfg(target_os = "macos")]
    fn on_accessibility_action(&self, callback: Box<dyn FnMut(accesskit::ActionRequest)>) {
        self.0.lock().accessibility_callback = Some(callback);
    }

    #[cfg(target_os = "macos")]
    fn update_accessibility(
        &self,
        nodes: &[crate::AccessibilityNode],
        error: Option<&'static str>,
    ) {
        self.0.lock().accessibility_nodes =
            if error.is_none() { nodes.to_vec() } else { Vec::new() };
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().active_status_change_callback = Some(callback)
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().hover_status_change_callback = Some(callback)
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.lock().resize_callback = Some(callback)
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().moved_callback = Some(callback)
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.lock().should_close_handler = Some(callback);
    }

    fn on_close(&self, _callback: Box<dyn FnOnce()>) {}

    fn on_hit_test_window_control(
        &self,
        callback: Box<dyn FnMut(Option<Point<Pixels>>) -> Option<WindowControlArea>>,
    ) {
        self.0.lock().hit_test_window_control_callback = Some(callback);
    }

    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, _scene: &crate::Scene) {}

    fn sprite_atlas(&self) -> sync::Arc<dyn crate::PlatformAtlas> {
        self.0.lock().sprite_atlas.clone()
    }

    fn as_test(&mut self) -> Option<&mut TestWindow> {
        Some(self)
    }

    #[cfg(target_os = "windows")]
    fn get_raw_handle(&self) -> windows::Win32::Foundation::HWND {
        unimplemented!()
    }

    fn show_window_menu(&self, _position: Point<Pixels>) {
        unimplemented!()
    }

    fn start_window_move(&self) {
        unimplemented!()
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
}

pub(crate) struct TestAtlasState {
    next_id: u32,
    tiles: HashMap<AtlasKey, AtlasTile>,
}

pub(crate) struct TestAtlas(Mutex<TestAtlasState>);

impl TestAtlas {
    pub fn new() -> Self {
        TestAtlas(Mutex::new(TestAtlasState { next_id: 0, tiles: HashMap::default() }))
    }
}

impl PlatformAtlas for TestAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &crate::AtlasKey,
        build: &mut dyn FnMut() -> anyhow::Result<
            Option<(Size<crate::DevicePixels>, std::borrow::Cow<'a, [u8]>)>,
        >,
    ) -> anyhow::Result<Option<crate::AtlasTile>> {
        let mut state = self.0.lock();
        if let Some(tile) = state.tiles.get(key) {
            return Ok(Some(tile.clone()));
        }
        drop(state);

        let Some((size, _)) = build()? else {
            return Ok(None);
        };

        let mut state = self.0.lock();
        state.next_id += 1;
        let texture_id = state.next_id;
        state.next_id += 1;
        let tile_id = state.next_id;

        state.tiles.insert(
            key.clone(),
            crate::AtlasTile {
                texture_id: AtlasTextureId {
                    index: texture_id,
                    kind: crate::AtlasTextureKind::Monochrome,
                },
                tile_id: TileId(tile_id),
                padding: 0,
                bounds: crate::Bounds { origin: Point::default(), size },
            },
        );

        Ok(Some(state.tiles[key].clone()))
    }

    fn remove(&self, key: &AtlasKey) {
        let mut state = self.0.lock();
        state.tiles.remove(key);
    }
}
