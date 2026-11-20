//! One current blob raster and one background read/decode, even across browser replacement.
//! Existing tool contexts retain file authority and exact request/reply correlation.
use super::*;
use crate::{
    desktop::worker_queue::Gate,
    engine::tool_client::DataContext,
    runtime::host::{DataCall, DataOperation, DataResult},
    store::volume_index::EntryAddress,
};
use gpui::{AnyWindowHandle, EntityId, Global, RenderImage, WeakEntity};
use image::ImageDecoder;
use std::{
    io::Cursor,
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const INPUT_BYTES_MAX: u64 = 8 * 1024 * 1024;
const SOURCE_DIMENSION_MAX: u32 = 8192;
const SOURCE_PIXELS_MAX: u32 = 16 * 1024 * 1024;
const DECODE_BYTES_MAX: u64 = 64 * 1024 * 1024;
const PROFILE_BYTES_MAX: usize = 64 * 1024;
const PREVIEW_SIDE_MAX: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    address: EntryAddress,
    revision: EntryId,
    len_bytes: u64,
    digest: [u8; 32],
    epoch: u64,
}

#[derive(Default)]
pub(super) struct Preview {
    generation: u64,
    target: Option<Target>,
    image: Option<Arc<RenderImage>>,
    window: Option<AnyWindowHandle>,
    loading: bool,
}

impl Preview {
    pub fn new(cx: &mut Context<Browser>) -> Self {
        let owner = cx.entity_id();
        cx.on_release(move |this, cx| {
            retire(owner, cx);
            this.preview.retire_image(cx);
        })
        .detach();
        Self::default()
    }

    fn retire_image(&mut self, cx: &mut App) {
        if let Some(image) = self.image.take()
            && let Some(window) = self.window
        {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, _| window.drop_image(image));
            });
        }
    }
}

struct Pending {
    owner: WeakEntity<Browser>,
    target: Target,
    generation: u64,
    gate: Arc<Gate>,
}

struct Active {
    owner: EntityId,
    cancellation: Arc<Cancellation>,
}

#[derive(Default)]
struct Coordinator {
    active: Option<Active>,
    latest: Option<Pending>,
    displayed: Option<WeakEntity<Browser>>,
}
impl Global for Coordinator {}

#[derive(Default)]
struct Cancellation {
    stopped: AtomicBool,
    socket: Mutex<Option<UnixStream>>,
}

impl Cancellation {
    fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(socket) =
            self.socket.lock().expect("preview socket mutex is not poisoned").take()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    fn check(&self, gate: &Gate, epoch: u64) -> crate::engine::Result<()> {
        if self.stopped.load(Ordering::Acquire) || !gate.accepts(epoch) {
            return Err("Blob preview was retired".into());
        }
        Ok(())
    }

    fn exchange(
        &self,
        profile: &Path,
        request: Request,
        gate: &Gate,
        epoch: u64,
    ) -> crate::engine::Result<Reply> {
        let lifecycle =
            matches!(request, Request::ToolContextOpen { .. } | Request::ToolContextClose { .. });
        if !matches!(request, Request::ToolContextClose { .. }) {
            self.check(gate, epoch)?;
        }
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        socket.set_nonblocking(true)?;
        socket.connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?)?;
        let mut socket: UnixStream = socket.into();
        socket.set_nonblocking(false)?;
        if !lifecycle {
            let mut active = self.socket.lock().expect("preview socket mutex is not poisoned");
            self.check(gate, epoch)?;
            *active = Some(socket.try_clone()?);
        }
        let response = ipc::call_stream_archive(&mut socket, &request);
        if !lifecycle {
            self.socket.lock().expect("preview socket mutex is not poisoned").take();
            self.check(gate, epoch)?;
        }
        // Keep lifecycle exchanges owned through their reply: cancellation must
        // not lose an admitted context's handle before its matching close.
        response?.into_owned()
    }
}

impl Browser {
    fn blob_preview_target(&self) -> Option<Target> {
        let details = self.details.as_ref()?;
        let EntryKind::Blob { len_bytes, digest, .. } = details.entry.info.kind else {
            return None;
        };
        let gate = self.send.gate();
        let epoch = gate.epoch();
        (self.selected == Some(details.entry.id)
            && !details.entry.deleted
            && details.readable
            && (1..=INPUT_BYTES_MAX).contains(&len_bytes)
            && gate.accepts(epoch))
        .then_some(Target {
            address: EntryAddress { volume: self.volume.id, entry: details.entry.id },
            revision: details.entry.content_revision,
            len_bytes,
            digest,
            epoch,
        })
    }

    pub(super) fn refresh_blob_preview(&mut self, cx: &mut Context<Self>) {
        let target = self.blob_preview_target();
        if self.preview.target == target {
            return;
        }
        self.clear_blob_preview(cx);
        self.preview.target = target;
        let Some(target) = target else { return };
        self.preview.loading = true;
        let displayed = cx.default_global::<Coordinator>().displayed.take();
        if let Some(displayed) = displayed
            && displayed.entity_id() != cx.entity_id()
        {
            let _ = displayed.update(cx, |this, cx| this.clear_blob_preview(cx));
        }
        let pending = Pending {
            owner: cx.weak_entity(),
            target,
            generation: self.preview.generation,
            gate: self.send.gate(),
        };
        let coordinator = cx.default_global::<Coordinator>();
        if let Some(active) = &coordinator.active {
            active.cancellation.cancel();
        }
        coordinator.latest = Some(pending);
        start_latest(cx);
    }

    pub(super) fn clear_blob_preview(&mut self, cx: &mut Context<Self>) {
        self.preview.generation =
            self.preview.generation.checked_add(1).expect("preview generation must not wrap");
        self.preview.target = None;
        self.preview.loading = false;
        self.preview.retire_image(cx);
        retire(cx.entity_id(), cx);
    }

    pub(super) fn blob_preview(
        &mut self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.preview.window = Some(window.window_handle());
        let content = if let Some(image) = &self.preview.image {
            gpui::img(image.clone()).size_full().into_any_element()
        } else if self.preview.loading {
            div().into_any_element()
        } else {
            div()
                .text_size(crate::ui::desktop::zoom::length(14.))
                .line_height(crate::ui::desktop::zoom::length(20.))
                .text_color(gpui::black().alpha(0.2))
                .child("No preview")
                .into_any_element()
        };
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .size_full()
            .p(crate::ui::desktop::zoom::length(8.))
            .flex()
            .items_center()
            .justify_center()
            .child(content)
    }
}

fn retire(owner: EntityId, cx: &mut App) {
    if !cx.has_global::<Coordinator>() {
        return;
    }
    let coordinator = cx.global_mut::<Coordinator>();
    if coordinator.displayed.as_ref().is_some_and(|displayed| displayed.entity_id() == owner) {
        coordinator.displayed = None;
    }
    if coordinator.latest.as_ref().is_some_and(|pending| pending.owner.entity_id() == owner) {
        coordinator.latest = None;
    }
    if let Some(active) = &coordinator.active
        && active.owner == owner
    {
        active.cancellation.cancel();
    }
}

fn start_latest(cx: &mut App) {
    let coordinator = cx.default_global::<Coordinator>();
    if coordinator.active.is_some() {
        return;
    }
    let Some(pending) = coordinator.latest.take() else { return };
    let Some(owner) = pending.owner.upgrade() else { return };
    let cancellation = Arc::new(Cancellation::default());
    cx.global_mut::<Coordinator>().active =
        Some(Active { owner: owner.entity_id(), cancellation: cancellation.clone() });
    let profile = crate::engine::profile_path();
    let target = pending.target;
    let gate = pending.gate.clone();
    let job = cx.background_executor().spawn(async move {
        load(&profile, target, &gate, &cancellation).map_err(|error| error.to_string())
    });
    cx.spawn(async move |cx| {
        let result = job.await;
        let _ = cx.update(|cx| {
            cx.global_mut::<Coordinator>().active = None;
            deliver(pending, result, cx);
            start_latest(cx);
        });
    })
    .detach();
}

fn deliver(pending: Pending, result: Result<Arc<RenderImage>, String>, cx: &mut App) {
    let installed = pending
        .owner
        .update(cx, |this, cx| {
            if this.preview.generation == pending.generation
                && this.preview.target == Some(pending.target)
            {
                if this.blob_preview_target() == Some(pending.target) {
                    this.preview.retire_image(cx);
                    this.preview.image = result.ok();
                }
                this.preview.loading = false;
                cx.notify();
                return this.preview.image.is_some();
            }
            false
        })
        .unwrap_or(false);
    if installed {
        cx.default_global::<Coordinator>().displayed = Some(pending.owner);
    }
}

fn load(
    profile: &Path,
    target: Target,
    gate: &Gate,
    cancellation: &Cancellation,
) -> crate::engine::Result<Arc<RenderImage>> {
    let exchange = |request| cancellation.exchange(profile, request, gate, target.epoch);
    let mut context = DataContext::open_volume(target.address.volume, &exchange)?;
    let mut execute = |operation| {
        context.execute(DataCall { volume: *target.address.volume.as_bytes(), operation })
    };
    let result = (|| {
        let bytes = read_bytes(target, &mut execute, || cancellation.check(gate, target.epoch))?;
        let image = decode(&bytes)
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
        cancellation.check(gate, target.epoch)?;
        check_metadata(target, &mut execute)?;
        Ok(image)
    })();
    let result = context.finish(result);
    // Finishing retains cleanup ownership even after cancellation. A success
    // prepared before that close cannot cross back into a later selection.
    cancellation.check(gate, target.epoch)?;
    result
}

fn check_metadata(
    target: Target,
    execute: &mut impl FnMut(DataOperation) -> crate::engine::Result<DataResult>,
) -> crate::engine::Result<()> {
    match execute(DataOperation::FileMetadata { entry_id: *target.address.entry.as_bytes() })? {
        DataResult::FileMetadata { len_bytes, digest }
            if len_bytes == target.len_bytes && digest == target.digest =>
        {
            Ok(())
        }
        _ => Err("Blob changed while preparing its preview".into()),
    }
}

fn read_bytes(
    target: Target,
    execute: &mut impl FnMut(DataOperation) -> crate::engine::Result<DataResult>,
    check: impl Fn() -> crate::engine::Result<()>,
) -> crate::engine::Result<Vec<u8>> {
    if !(1..=INPUT_BYTES_MAX).contains(&target.len_bytes) {
        return Err("Blob exceeds the preview byte limit".into());
    }
    check()?;
    check_metadata(target, execute)?;
    let mut bytes = Vec::with_capacity(target.len_bytes as usize);
    let mut hash = blake3::Hasher::new();
    while (bytes.len() as u64) < target.len_bytes {
        check()?;
        let DataResult::File(chunk) = execute(DataOperation::FileRead {
            entry_id: *target.address.entry.as_bytes(),
            offset: bytes.len() as u64,
            limit: crate::store::BLOB_CHUNK_BYTES as u32,
        })?
        else {
            return Err("Unexpected blob preview chunk".into());
        };
        if chunk.total_bytes != target.len_bytes
            || chunk.bytes.is_empty()
            || chunk.bytes.len() > crate::store::BLOB_CHUNK_BYTES
            || chunk.bytes.len() as u64 > target.len_bytes - bytes.len() as u64
        {
            return Err("Blob changed while preparing its preview".into());
        }
        hash.update(&chunk.bytes);
        bytes.extend_from_slice(&chunk.bytes);
    }
    if *hash.finalize().as_bytes() != target.digest {
        return Err("Blob preview content does not match its digest".into());
    }
    check()?;
    check_metadata(target, execute)?;
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<Arc<RenderImage>, String> {
    if bytes.is_empty() || bytes.len() as u64 > INPUT_BYTES_MAX {
        return Err("Blob exceeds the preview byte limit".into());
    }
    let format = image::guess_format(bytes).map_err(|error| error.to_string())?;
    if !matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg) {
        return Err("This file has no image preview".into());
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(SOURCE_DIMENSION_MAX);
    limits.max_image_height = Some(SOURCE_DIMENSION_MAX);
    limits.max_alloc = Some(DECODE_BYTES_MAX);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width.checked_mul(height).is_none_or(|pixels| pixels > SOURCE_PIXELS_MAX)
        || !matches!(
            decoder.color_type(),
            image::ColorType::L8
                | image::ColorType::La8
                | image::ColorType::Rgb8
                | image::ColorType::Rgba8
        )
    {
        return Err("Image exceeds the preview pixel limit".into());
    }
    let grayscale = matches!(decoder.color_type(), image::ColorType::L8 | image::ColorType::La8);
    let orientation = decoder.orientation().map_err(|error| error.to_string())?;
    let profile = decoder.icc_profile().map_err(|error| error.to_string())?;
    if profile.as_ref().is_some_and(|profile| profile.len() > PROFILE_BYTES_MAX) {
        return Err("Image profile exceeds the preview limit".into());
    }
    let source = image::DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    let rgba = thumbnail(source, orientation);
    upload(rgba, profile.as_deref(), grayscale)
}

fn thumbnail(
    mut source: image::DynamicImage,
    orientation: image::metadata::Orientation,
) -> image::RgbaImage {
    if let image::DynamicImage::ImageRgba8(image) = &mut source {
        for pixel in image.pixels_mut() {
            let alpha = u16::from(pixel.0[3]);
            for channel in &mut pixel.0[..3] {
                *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
            }
        }
    } else if let image::DynamicImage::ImageLumaA8(image) = &mut source {
        for pixel in image.pixels_mut() {
            pixel.0[0] = ((u16::from(pixel.0[0]) * u16::from(pixel.0[1]) + 127) / 255) as u8;
        }
    }
    let premultiplied =
        matches!(source, image::DynamicImage::ImageRgba8(_) | image::DynamicImage::ImageLumaA8(_));
    let mut thumbnail = if source.width() <= PREVIEW_SIDE_MAX && source.height() <= PREVIEW_SIDE_MAX
    {
        source
    } else {
        let thumbnail = source.thumbnail(PREVIEW_SIDE_MAX, PREVIEW_SIDE_MAX);
        drop(source);
        thumbnail
    };
    thumbnail.apply_orientation(orientation);
    let mut rgba = thumbnail.into_rgba8();
    if premultiplied {
        for pixel in rgba.pixels_mut().filter(|pixel| pixel.0[3] != 0) {
            let alpha = u32::from(pixel.0[3]);
            for channel in &mut pixel.0[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    rgba
}

fn upload(
    mut rgba: image::RgbaImage,
    profile: Option<&[u8]>,
    grayscale: bool,
) -> Result<Arc<RenderImage>, String> {
    let source = profile
        .map_or_else(
            || Ok(moxcms::ColorProfile::new_srgb()),
            |profile| {
                moxcms::ColorProfile::new_from_slice_with_options(
                    profile,
                    moxcms::ParsingOptions {
                        max_profile_size: PROFILE_BYTES_MAX + 1,
                        max_allowed_clut_size: 1024 * 1024,
                        max_allowed_trc_size: 4096,
                    },
                )
            },
        )
        .map_err(|error| error.to_string())?;
    if source.color_space == moxcms::DataColorSpace::Gray {
        if !grayscale {
            return Err("Image profile does not match its color channels".into());
        }
        upload_gray(&mut rgba, &source)?;
    } else {
        let transform = source
            .create_in_place_transform_8bit(
                moxcms::Layout::Rgba,
                &moxcms::ColorProfile::new_display_p3(),
                Default::default(),
            )
            .map_err(|error| error.to_string())?;
        transform.transform(&mut rgba).map_err(|error| error.to_string())?;
    }
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([image::Frame::new(rgba)])))
}

fn upload_gray(rgba: &mut image::RgbaImage, source: &moxcms::ColorProfile) -> Result<(), String> {
    // Thumbnail expansion preserves the gray sample in each RGB channel. The
    // actual Gray ICC transform needs GrayAlpha input, at most another 2 MiB.
    let mut gray = Vec::with_capacity(rgba.as_raw().len() / 2);
    for pixel in rgba.pixels() {
        gray.extend_from_slice(&[pixel.0[0], pixel.0[3]]);
    }
    source
        .create_transform_8bit(
            moxcms::Layout::GrayAlpha,
            &moxcms::ColorProfile::new_display_p3(),
            moxcms::Layout::Rgba,
            Default::default(),
        )
        .map_err(|error| error.to_string())?
        .transform(&gray, rgba)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert exact preview admission and pixel results.")]
mod tests {
    use super::*;
    use crate::runtime::host::FileChunk;
    use std::cell::Cell;

    fn target(bytes: &[u8]) -> Target {
        Target {
            address: EntryAddress {
                volume: Id::from_bytes([1; 16]),
                entry: EntryId::from_bytes([2; 15]).unwrap(),
            },
            revision: EntryId::from_bytes([3; 15]).unwrap(),
            len_bytes: bytes.len() as u64,
            digest: *blake3::hash(bytes).as_bytes(),
            epoch: 0,
        }
    }

    fn raster(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
        let source = image::RgbImage::from_pixel(width, height, image::Rgb([255, 0, 0]));
        let mut bytes = Cursor::new(Vec::new());
        source.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    fn decode_small_stack(bytes: Vec<u8>) -> Result<Arc<RenderImage>, String> {
        // Bound the test worker's stack without initializing AppKit from a
        // libtest thread. Packaged native UI checks exercise production GCD.
        std::thread::Builder::new()
            .name("preview-decode-test".into())
            .stack_size(512 * 1024)
            .spawn(move || decode(&bytes))
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn raster_preview_preserves_aspect_alpha_and_p3_channel_order() {
        // Check color conversion with the same stack ceiling used by the small-worker regression.
        for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg] {
            let bytes = raster(2048, 1024, format);
            let preview = decode_small_stack(bytes).unwrap();
            assert_eq!(
                preview.size(0),
                gpui::size(gpui::DevicePixels(1024), gpui::DevicePixels(512))
            );
            assert_eq!(preview.frame_count(), 1);
            let pixels = preview.as_bytes(0).unwrap();
            assert_eq!(pixels.len(), 1024 * 512 * 4);
            for pixel in pixels.as_chunks::<4>().0 {
                assert!((i32::from(pixel[0]) - 35).abs() <= 2);
                assert!((i32::from(pixel[1]) - 51).abs() <= 2);
                assert!((i32::from(pixel[2]) - 234).abs() <= 2);
                assert_eq!(pixel[3], 255);
            }
        }
        let mut source = image::RgbaImage::from_pixel(2048, 2, image::Rgba([0, 0, 255, 0]));
        for y in 0..2 {
            source.put_pixel(0, y, image::Rgba([255, 0, 0, 255]));
        }
        let sampled = thumbnail(
            image::DynamicImage::ImageRgba8(source),
            image::metadata::Orientation::NoTransforms,
        );
        let first = sampled.get_pixel(0, 0).0;
        assert!(first[0] > 250 && first[2] == 0 && first[3] > 0 && first[3] < 255);
        let turned = thumbnail(
            image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 2)),
            image::metadata::Orientation::Rotate90,
        );
        assert_eq!(turned.dimensions(), (2, 4));
    }

    #[test]
    fn preview_decode_rejects_unsupported_bytes_dimensions_pixels_and_depth() {
        // Resource admission rejects encoded and decoded extremes without allocating a giant raster.
        assert!(decode(b"not an image").is_err());
        assert!(decode(b"GIF89a\x01\0\x01\0").is_err());
        assert!(decode(&vec![0; INPUT_BYTES_MAX as usize + 1]).is_err());
        assert!(decode(&raster(SOURCE_DIMENSION_MAX + 1, 1, image::ImageFormat::Png)).is_err());
        // Patch only the PNG IHDR and its CRC: the pixel-product gate precedes pixel decompression.
        let mut excessive = raster(1, 1, image::ImageFormat::Png);
        excessive[16..20].copy_from_slice(&8192u32.to_be_bytes());
        excessive[20..24].copy_from_slice(&8192u32.to_be_bytes());
        let mut crc = !0u32;
        for byte in &excessive[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        excessive[29..33].copy_from_slice(&(!crc).to_be_bytes());
        assert!(decode(&excessive).is_err());
        let mut deep = Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Luma([65535u16]),
        ))
        .write_to(&mut deep, image::ImageFormat::Png)
        .unwrap();
        assert!(decode(deep.get_ref()).is_err());
    }

    #[test]
    fn preview_reads_require_exact_metadata_chunks_digest_and_final_authority() {
        // Correlated data outcomes cannot install partial, replaced or newly unreadable content.
        let bytes = vec![37; crate::store::BLOB_CHUNK_BYTES + 11];
        let target = target(&bytes);
        for failure in [
            None,
            Some("preflight"),
            Some("chunk"),
            Some("digest"),
            Some("final denial"),
            Some("final change"),
        ] {
            let metadata = Cell::new(0);
            let mut execute = |operation| -> crate::engine::Result<DataResult> {
                match operation {
                    DataOperation::FileMetadata { entry_id } => {
                        assert_eq!(entry_id, *target.address.entry.as_bytes());
                        metadata.set(metadata.get() + 1);
                        if failure == Some("final denial") && metadata.get() == 2 {
                            return Err(Box::new(crate::engine::tool_client::DataError(
                                playbit_api::abi::syscall::Error::AccessDenied,
                            )));
                        }
                        let changed = failure == Some("preflight")
                            || failure == Some("final change") && metadata.get() == 2;
                        Ok(DataResult::FileMetadata {
                            len_bytes: target.len_bytes + u64::from(changed),
                            digest: target.digest,
                        })
                    }
                    DataOperation::FileRead { entry_id, offset, limit } => {
                        assert_eq!(entry_id, *target.address.entry.as_bytes());
                        assert_eq!(limit, crate::store::BLOB_CHUNK_BYTES as u32);
                        let end = (offset as usize + limit as usize).min(bytes.len());
                        let mut chunk = bytes[offset as usize..end].to_vec();
                        if failure == Some("digest") {
                            chunk[0] ^= 1;
                        }
                        Ok(DataResult::File(FileChunk {
                            total_bytes: target.len_bytes + u64::from(failure == Some("chunk")),
                            bytes: chunk,
                        }))
                    }
                    _ => panic!("preview uses only metadata and bounded file reads"),
                }
            };
            let result = read_bytes(target, &mut execute, || Ok(()));
            assert_eq!(result.is_ok(), failure.is_none(), "{failure:?}");
            if failure.is_none() {
                assert_eq!(result.unwrap(), bytes);
            } else if failure == Some("final denial") {
                assert!(
                    result
                        .unwrap_err()
                        .downcast_ref::<crate::engine::tool_client::DataError>()
                        .is_some()
                );
            }
        }
        let mut no_work = |_| -> crate::engine::Result<DataResult> {
            panic!("retired or over-limit reads cannot execute")
        };
        assert!(
            read_bytes(
                Target { len_bytes: INPUT_BYTES_MAX + 1, ..target },
                &mut no_work,
                || Ok(())
            )
            .is_err()
        );
        assert!(read_bytes(target, &mut no_work, || Err("retired".into())).is_err());
    }

    #[test]
    fn retirement_closes_only_its_owned_socket_and_keeps_epoch_fences() {
        // Closing the registered duplicate interrupts an in-flight read without joining a worker.
        use std::io::Read;
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let cancellation = Cancellation::default();
        *cancellation.socket.lock().unwrap() = Some(socket);
        let gate = Gate::default();
        assert!(cancellation.check(&gate, 0).is_ok());
        cancellation.cancel();
        assert!(cancellation.check(&gate, 0).is_err());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        let cancellation = Cancellation::default();
        gate.pause();
        gate.resume();
        assert!(cancellation.check(&gate, 0).is_err());
        assert!(cancellation.check(&gate, gate.epoch()).is_ok());
    }

    fn select_blob(browser: &mut Browser, byte: u8) {
        let mut details = super::super::tests::details(byte, EntryAcl::Inherit);
        details.entry.info.kind = EntryKind::Blob {
            len_bytes: 4,
            digest: *blake3::hash(b"file").as_bytes(),
            encrypted_key: [1; 32],
            published_at_micros: 1,
        };
        details.entry.info.protocol = None;
        browser.selected = Some(details.entry.id);
        browser.details = Some(Arc::new(details));
    }

    #[gpui::test]
    fn preview_admission_keeps_one_active_and_only_the_latest_browser_target(
        cx: &mut gpui::TestAppContext,
    ) {
        // Hold an active job while targets and Browser entities change: no retired-job queue grows.
        let (send, _requests, _) = crate::desktop::worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(super::super::tests::volume(1), send.clone(), cx));
        let replacement = cx.new(|cx| Browser::new(super::super::tests::volume(2), send, cx));
        let cancellation = Arc::new(Cancellation::default());
        cx.update(|cx| {
            cx.set_global(Coordinator {
                active: Some(Active {
                    owner: browser.entity_id(),
                    cancellation: cancellation.clone(),
                }),
                latest: None,
                displayed: None,
            })
        });
        for byte in [3, 4] {
            browser.update(cx, |this, cx| {
                select_blob(this, byte);
                this.refresh_blob_preview(cx);
            });
        }
        replacement.update(cx, |this, cx| {
            select_blob(this, 5);
            this.refresh_blob_preview(cx);
        });
        cx.update(|cx| {
            let coordinator = cx.global::<Coordinator>();
            assert!(Arc::ptr_eq(&coordinator.active.as_ref().unwrap().cancellation, &cancellation));
            assert!(cancellation.stopped.load(Ordering::Acquire));
            let latest = coordinator.latest.as_ref().unwrap();
            assert_eq!(latest.owner.entity_id(), replacement.entity_id());
            assert_eq!(latest.target.address.entry, EntryId::from_bytes([5; 15]).unwrap());
            retire(browser.entity_id(), cx);
            assert!(cx.global::<Coordinator>().latest.is_some());
            retire(replacement.entity_id(), cx);
            assert!(cx.global::<Coordinator>().latest.is_none());
        });
    }

    #[gpui::test]
    fn preview_delivery_requires_current_selection_revision_read_authority_and_epoch(
        cx: &mut gpui::TestAppContext,
    ) {
        // A successful raster is display-only data, never authority to bypass a later state change.
        let (send, requests, _) = crate::desktop::worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(super::super::tests::volume(1), send, cx));
        let image = Arc::new(RenderImage::new([image::Frame::new(image::RgbaImage::new(1, 1))]));
        for change in ["current", "selection", "revision", "permission", "deleted", "epoch"] {
            let target = browser.update(cx, |this, _| {
                select_blob(this, 3);
                this.send.gate().resume();
                let target = this.blob_preview_target().unwrap();
                this.preview.target = Some(target);
                this.preview.image = None;
                this.preview.loading = true;
                match change {
                    "selection" => this.selected = None,
                    "revision" => {
                        Arc::make_mut(this.details.as_mut().unwrap()).entry.content_revision =
                            EntryId::from_bytes([9; 15]).unwrap()
                    }
                    "permission" => Arc::make_mut(this.details.as_mut().unwrap()).readable = false,
                    "deleted" => Arc::make_mut(this.details.as_mut().unwrap()).entry.deleted = true,
                    "epoch" => {
                        this.send.gate().pause();
                        this.send.gate().resume();
                    }
                    _ => {}
                }
                target
            });
            let gate = browser.read_with(cx, |this, _| this.send.gate());
            let generation = browser.read_with(cx, |this, _| this.preview.generation);
            cx.update(|cx| {
                deliver(
                    Pending { owner: browser.downgrade(), target, generation, gate },
                    Ok(image.clone()),
                    cx,
                )
            });
            browser.read_with(cx, |this, _| {
                assert_eq!(this.preview.image.is_some(), change == "current", "{change}");
                assert!(!this.preview.loading);
            });
            if change == "current" {
                while requests.try_recv().is_ok() {}
                browser.update(cx, |this, cx| {
                    let details = this.details.clone().unwrap();
                    this.select(target.address.entry, cx);
                    assert!(Arc::ptr_eq(this.details.as_ref().unwrap(), &details));
                    assert!(Arc::ptr_eq(this.preview.image.as_ref().unwrap(), &image));
                    assert_eq!(this.preview.generation, generation);
                    assert!(!this.preview.loading);
                });
                assert!(
                    requests.try_recv().is_err(),
                    "reselecting a loaded blob must keep its preview"
                );
            }
        }
    }

    #[gpui::test]
    fn returning_to_the_same_blob_rejects_an_earlier_selection_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        // A1 can finish its uncancelable close after A→B→A2. Equal file identity is insufficient.
        let (send, _requests, _) = crate::desktop::worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(super::super::tests::volume(1), send, cx));
        cx.update(|cx| {
            cx.set_global(Coordinator {
                active: Some(Active {
                    owner: browser.entity_id(),
                    cancellation: Arc::new(Cancellation::default()),
                }),
                latest: None,
                displayed: None,
            })
        });
        browser.update(cx, |this, cx| {
            select_blob(this, 3);
            this.refresh_blob_preview(cx);
        });
        let first = cx.update(|cx| cx.global_mut::<Coordinator>().latest.take().unwrap());
        for byte in [4, 3] {
            browser.update(cx, |this, cx| {
                select_blob(this, byte);
                this.refresh_blob_preview(cx);
            });
        }
        let latest = cx.update(|cx| cx.global_mut::<Coordinator>().latest.take().unwrap());
        assert_eq!(first.target, latest.target);
        assert!(first.generation < latest.generation);
        let old = Arc::new(RenderImage::new([image::Frame::new(image::RgbaImage::new(1, 1))]));
        let current = Arc::new(RenderImage::new([image::Frame::new(image::RgbaImage::new(2, 1))]));
        cx.update(|cx| deliver(first, Ok(old), cx));
        browser.read_with(cx, |this, _| {
            assert!(this.preview.image.is_none());
            assert!(this.preview.loading, "A1 cannot settle A2's loading state");
        });
        cx.update(|cx| deliver(latest, Ok(current.clone()), cx));
        browser.read_with(cx, |this, _| {
            assert!(Arc::ptr_eq(this.preview.image.as_ref().unwrap(), &current));
            assert!(!this.preview.loading);
        });
    }

    #[test]
    fn grayscale_icc_png_and_jpeg_transform_the_gray_samples_and_preserve_alpha() {
        // A linear Gray ICC profile must brighten mid-gray to P3's transfer curve, not disappear.
        use image::ImageEncoder;
        let profile = moxcms::ColorProfile::new_gray_with_gamma(1.).encode().unwrap();
        for (format, alpha) in [
            (image::ImageFormat::Png, None),
            (image::ImageFormat::Png, Some(128)),
            (image::ImageFormat::Jpeg, None),
        ] {
            let mut bytes = Vec::new();
            let mut samples = vec![128];
            if let Some(alpha) = alpha {
                samples.push(alpha);
            }
            if format == image::ImageFormat::Png {
                let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
                encoder.set_icc_profile(profile.clone()).unwrap();
                encoder
                    .write_image(
                        &samples,
                        1,
                        1,
                        if alpha.is_some() {
                            image::ExtendedColorType::La8
                        } else {
                            image::ExtendedColorType::L8
                        },
                    )
                    .unwrap();
            } else {
                let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut bytes);
                encoder.set_icc_profile(profile.clone()).unwrap();
                encoder.encode(&samples, 1, 1, image::ExtendedColorType::L8).unwrap();
            }
            let preview = decode_small_stack(bytes).unwrap();
            let pixel = preview.as_bytes(0).unwrap();
            assert_eq!(pixel.len(), 4);
            for channel in &pixel[..3] {
                assert!((i32::from(*channel) - 188).abs() <= 3, "{format:?} {pixel:?}");
            }
            assert_eq!(pixel[3], alpha.unwrap_or(255));
        }
    }
}
