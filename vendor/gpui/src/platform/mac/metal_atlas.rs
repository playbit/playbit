use crate::{
    AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, DevicePixels, PlatformAtlas,
    Point, Scene, Size, platform::AtlasTextureList,
};
use anyhow::{Context as _, Result};
use collections::FxHashMap;
use derive_more::{Deref, DerefMut};
use etagere::BucketedAtlasAllocator;
use metal::Device;
use parking_lot::Mutex;
use std::borrow::Cow;

pub(crate) struct MetalAtlas(Mutex<MetalAtlasState>);

fn texture_size(min_size: Size<DevicePixels>, kind: AtlasTextureKind) -> Size<DevicePixels> {
    // Tiny RGBA controls should not reserve four MiB. Glyph pages keep their
    // existing size; larger images still determine their own minimum extent.
    let edge = match kind {
        AtlasTextureKind::Monochrome => 1024,
        AtlasTextureKind::Polychrome => 256,
    };
    let default = Size { width: DevicePixels(edge), height: DevicePixels(edge) };
    // Max texture size on modern Apple GPUs; larger dimensions crash validation.
    let maximum = Size { width: DevicePixels(16384), height: DevicePixels(16384) };
    min_size.min(&maximum).max(&default)
}

impl MetalAtlas {
    pub(crate) fn new(device: Device) -> Self {
        MetalAtlas(Mutex::new(MetalAtlasState {
            device: AssertSend(device),
            monochrome_textures: Default::default(),
            polychrome_textures: Default::default(),
            tiles_by_key: Default::default(),
        }))
    }

    /// The finished CPU scene is the sole owner of reusable tile identities.
    /// Encoded Metal command buffers retain their textures independently.
    pub(crate) fn prepare_scene(&self, scene: &Scene) {
        let mut guard = self.0.lock();
        let state = &mut *guard;
        for texture in
            state.monochrome_textures.iter_mut().chain(state.polychrome_textures.iter_mut())
        {
            texture.in_scene = false;
        }
        for id in scene
            .monochrome_sprites
            .iter()
            .map(|sprite| sprite.tile.texture_id)
            .chain(scene.polychrome_sprites.iter().map(|sprite| sprite.tile.texture_id))
        {
            state.texture_mut(id).in_scene = true;
        }
        let mut retired = false;
        for textures in [&mut state.monochrome_textures, &mut state.polychrome_textures] {
            for (index, slot) in textures.textures.iter_mut().enumerate() {
                if slot.as_ref().is_some_and(|texture| !texture.in_scene) {
                    *slot = None;
                    textures.free_list.push(index);
                    retired = true;
                }
            }
        }
        if retired {
            let MetalAtlasState { monochrome_textures, polychrome_textures, tiles_by_key, .. } =
                state;
            tiles_by_key.retain(|_, tile| {
                let textures = match tile.texture_id.kind {
                    AtlasTextureKind::Monochrome => &monochrome_textures.textures,
                    AtlasTextureKind::Polychrome => &polychrome_textures.textures,
                };
                textures[tile.texture_id.index as usize].is_some()
            });
            // Turnover must not preserve the largest historical key table.
            let target = tiles_by_key.len().max(1024);
            if tiles_by_key.capacity() > target * 2 {
                tiles_by_key.shrink_to(target);
            }
        }
    }

    pub(crate) fn metal_texture(&self, id: AtlasTextureId) -> metal::Texture {
        self.0.lock().texture(id).metal_texture.clone()
    }
}

struct MetalAtlasState {
    device: AssertSend<Device>,
    monochrome_textures: AtlasTextureList<MetalAtlasTexture>,
    polychrome_textures: AtlasTextureList<MetalAtlasTexture>,
    tiles_by_key: FxHashMap<AtlasKey, AtlasTile>,
}

impl PlatformAtlas for MetalAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut lock = self.0.lock();
        if let Some(tile) = lock.tiles_by_key.get(key) {
            Ok(Some(tile.clone()))
        } else {
            let Some((size, bytes)) = build()? else {
                return Ok(None);
            };
            let tile = match key {
                AtlasKey::PixelImage(_) => lock.allocate_pixels(size),
                _ => lock.allocate(size, key.texture_kind()),
            }
            .context("failed to allocate")?;
            let texture = lock.texture(tile.texture_id);
            texture.upload(tile.bounds, &bytes);
            lock.tiles_by_key.insert(key.clone(), tile.clone());
            Ok(Some(tile))
        }
    }

    fn remove(&self, key: &AtlasKey) {
        // Removal may happen during painting. The old or newly built scene can
        // still contain this tile; page retirement belongs to prepare_scene.
        self.0.lock().tiles_by_key.remove(key);
    }
}

impl MetalAtlasState {
    fn allocate(
        &mut self,
        size: Size<DevicePixels>,
        texture_kind: AtlasTextureKind,
    ) -> Option<AtlasTile> {
        {
            let textures = match texture_kind {
                AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
                AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
            };

            if let Some(tile) = textures.iter_mut().rev().find_map(|texture| texture.allocate(size))
            {
                return Some(tile);
            }
        }

        let extent = texture_size(size, texture_kind);
        let allocator = BucketedAtlasAllocator::new(extent.into());
        let texture = self.push_texture(extent, texture_kind, Some(allocator));
        texture.allocate(size)
    }

    fn allocate_pixels(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        if size.width.0 <= 0 || size.height.0 <= 0 || size.width.0 > 16384 || size.height.0 > 16384
        {
            return None;
        }
        // The entire exact-size texture belongs to one pixel frame. Shared
        // sprites cannot pin dead allocations, and no live texture is rewritten.
        let texture = self.push_texture(size, AtlasTextureKind::Polychrome, None);
        Some(AtlasTile {
            texture_id: texture.id,
            tile_id: crate::TileId(0),
            bounds: Bounds { origin: Point::default(), size },
            padding: 0,
        })
    }

    fn push_texture(
        &mut self,
        size: Size<DevicePixels>,
        kind: AtlasTextureKind,
        allocator: Option<BucketedAtlasAllocator>,
    ) -> &mut MetalAtlasTexture {
        let texture_descriptor = metal::TextureDescriptor::new();
        texture_descriptor.set_width(size.width.into());
        texture_descriptor.set_height(size.height.into());
        let pixel_format;
        let usage;
        match kind {
            AtlasTextureKind::Monochrome => {
                pixel_format = metal::MTLPixelFormat::A8Unorm;
                usage = metal::MTLTextureUsage::ShaderRead;
            }
            AtlasTextureKind::Polychrome => {
                pixel_format = metal::MTLPixelFormat::BGRA8Unorm;
                usage = metal::MTLTextureUsage::ShaderRead;
            }
        }
        texture_descriptor.set_pixel_format(pixel_format);
        texture_descriptor.set_usage(usage);
        let metal_texture = self.device.new_texture(&texture_descriptor);

        let texture_list = match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
        };

        let index = texture_list.free_list.pop();

        let atlas_texture = MetalAtlasTexture {
            id: AtlasTextureId { index: index.unwrap_or(texture_list.textures.len()) as u32, kind },
            allocator,
            metal_texture: AssertSend(metal_texture),
            in_scene: false,
        };

        if let Some(ix) = index {
            texture_list.textures[ix] = Some(atlas_texture);
            texture_list.textures.get_mut(ix)
        } else {
            texture_list.textures.push(Some(atlas_texture));
            texture_list.textures.last_mut()
        }
        .unwrap()
        .as_mut()
        .unwrap()
    }

    fn texture_mut(&mut self, id: AtlasTextureId) -> &mut MetalAtlasTexture {
        let textures = match id.kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
        };
        textures.textures[id.index as usize].as_mut().unwrap()
    }

    fn texture(&self, id: AtlasTextureId) -> &MetalAtlasTexture {
        let textures = match id.kind {
            crate::AtlasTextureKind::Monochrome => &self.monochrome_textures,
            crate::AtlasTextureKind::Polychrome => &self.polychrome_textures,
        };
        textures[id.index as usize].as_ref().unwrap()
    }
}

struct MetalAtlasTexture {
    id: AtlasTextureId,
    // None reserves this exact-size texture exclusively for a pixel frame.
    allocator: Option<BucketedAtlasAllocator>,
    metal_texture: AssertSend<metal::Texture>,
    in_scene: bool,
}

impl MetalAtlasTexture {
    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self.allocator.as_mut()?.allocate(size.into())?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            bounds: Bounds { origin: allocation.rectangle.min.into(), size },
            padding: 0,
        };
        Some(tile)
    }

    fn upload(&self, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
        let region = metal::MTLRegion::new_2d(
            bounds.origin.x.into(),
            bounds.origin.y.into(),
            bounds.size.width.into(),
            bounds.size.height.into(),
        );
        self.metal_texture.replace_region(
            region,
            0,
            bytes.as_ptr() as *const _,
            bounds.size.width.to_bytes(self.bytes_per_pixel()) as u64,
        );
    }

    fn bytes_per_pixel(&self) -> u8 {
        use metal::MTLPixelFormat::*;
        match self.metal_texture.pixel_format() {
            A8Unorm | R8Unorm => 1,
            RGBA8Unorm | BGRA8Unorm => 4,
            _ => unimplemented!(),
        }
    }
}

impl From<Size<DevicePixels>> for etagere::Size {
    fn from(size: Size<DevicePixels>) -> Self {
        etagere::Size::new(size.width.into(), size.height.into())
    }
}

impl From<etagere::Point> for Point<DevicePixels> {
    fn from(value: etagere::Point) -> Self {
        Point { x: DevicePixels::from(value.x), y: DevicePixels::from(value.y) }
    }
}

impl From<etagere::Size> for Size<DevicePixels> {
    fn from(size: etagere::Size) -> Self {
        Size { width: DevicePixels::from(size.width), height: DevicePixels::from(size.height) }
    }
}

impl From<etagere::Rectangle> for Bounds<DevicePixels> {
    fn from(rectangle: etagere::Rectangle) -> Self {
        Bounds { origin: rectangle.min.into(), size: rectangle.size().into() }
    }
}

#[derive(Deref, DerefMut)]
struct AssertSend<T>(T);

unsafe impl<T> Send for AssertSend<T> {}

#[cfg(test)]
mod allocation_tests {
    use super::*;

    fn size(width: i32, height: i32) -> Size<DevicePixels> {
        Size { width: DevicePixels(width), height: DevicePixels(height) }
    }

    #[test]
    fn playbit_colour_page_packs_original_trace_controls_and_preserves_image_extents() {
        let extent = texture_size(size(17, 19), AtlasTextureKind::Polychrome);
        assert_eq!(extent, size(256, 256));
        let mut allocator = etagere::BucketedAtlasAllocator::new(extent.into());
        let mut occupied = Vec::new();
        for image in [size(13, 16), size(14, 19), size(17, 16), size(7, 11)] {
            let allocation = allocator.allocate(image.into()).expect("original control fits");
            assert!(allocation.rectangle.max.x <= extent.width.0);
            assert!(allocation.rectangle.max.y <= extent.height.0);
            for previous in &occupied {
                assert!(!allocation.rectangle.intersects(previous));
            }
            occupied.push(allocation.rectangle);
        }
        assert!(allocator.allocate(size(512, 256).into()).is_none());
        let large = texture_size(size(512, 256), AtlasTextureKind::Polychrome);
        let mut next = etagere::BucketedAtlasAllocator::new(large.into());
        assert!(next.allocate(size(512, 256).into()).is_some());
    }

    #[test]
    fn playbit_glyph_minimum_large_images_and_device_maximum_are_preserved() {
        assert_eq!(texture_size(size(17, 19), AtlasTextureKind::Monochrome), size(1024, 1024));
        assert_eq!(texture_size(size(400, 160), AtlasTextureKind::Polychrome), size(400, 256));
        assert_eq!(texture_size(size(1200, 300), AtlasTextureKind::Monochrome), size(1200, 1024));
        for kind in [AtlasTextureKind::Monochrome, AtlasTextureKind::Polychrome] {
            assert_eq!(texture_size(size(16385, 16385), kind), size(16384, 16384));
        }
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::{
        ContentMask, FontId, GlyphId, MonochromeSprite, RenderGlyphParams, ScaledPixels, point, px,
        size,
    };
    use foreign_types::ForeignType;

    fn key(id: u32) -> AtlasKey {
        RenderGlyphParams {
            font_id: FontId(0),
            glyph_id: GlyphId(id),
            font_size: px(13.),
            subpixel_variant: point(0, 0),
            scale_factor: 1.,
            is_emoji: false,
        }
        .into()
    }

    fn insert(atlas: &MetalAtlas, id: u32, edge: i32, byte: u8) -> AtlasTile {
        atlas
            .get_or_insert_with(&key(id), &mut || {
                Ok(Some((
                    size(DevicePixels(edge), DevicePixels(edge)),
                    Cow::Owned(vec![byte; (edge * edge) as usize]),
                )))
            })
            .unwrap()
            .unwrap()
    }

    fn scene(tile: AtlasTile) -> Scene {
        let mut scene = Scene::default();
        let bounds = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(0.)),
            size(ScaledPixels(64.), ScaledPixels(64.)),
        );
        scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds,
            content_mask: ContentMask { bounds },
            color: crate::white(),
            tile,
            transformation: Default::default(),
        });
        scene.finish();
        scene
    }

    #[test]
    fn playbit_replayed_scene_keeps_tiles_then_empty_scene_retires_pages_and_keys() {
        objc::rc::autoreleasepool(|| {
            let atlas = MetalAtlas::new(Device::system_default().expect("Metal device"));
            let first = insert(&atlas, 1, 1024, 7);
            let second = insert(&atlas, 2, 1024, 8);
            let original = scene(first.clone());
            let mut replayed = Scene::default();
            replayed.replay(0..original.len(), &original);
            replayed.finish();
            atlas.prepare_scene(&replayed);
            let state = atlas.0.lock();
            assert_eq!(state.tiles_by_key.len(), 1);
            assert_eq!(state.tiles_by_key.get(&key(1)), Some(&first));
            assert!(state.monochrome_textures.textures[second.texture_id.index as usize].is_none());
            drop(state);
            assert_eq!(insert(&atlas, 1, 1024, 9), first);
            atlas.prepare_scene(&Scene::default());
            assert!(atlas.0.lock().tiles_by_key.is_empty());
            assert!(atlas.0.lock().monochrome_textures.textures.iter().all(Option::is_none));
        });
    }

    #[test]
    fn playbit_repeated_key_removal_preserves_shared_page_and_current_scene() {
        objc::rc::autoreleasepool(|| {
            let atlas = MetalAtlas::new(Device::system_default().expect("Metal device"));
            let first = insert(&atlas, 1, 16, 7);
            let second = insert(&atlas, 2, 16, 8);
            assert_eq!(first.texture_id, second.texture_id);
            atlas.remove(&key(1));
            atlas.remove(&key(1));
            atlas.prepare_scene(&scene(first));
            assert!(!atlas.0.lock().tiles_by_key.contains_key(&key(1)));
            assert_eq!(insert(&atlas, 2, 16, 9), second);
            atlas.prepare_scene(&Scene::default());
            assert!(atlas.0.lock().tiles_by_key.is_empty());
        });
    }

    #[test]
    fn playbit_polychrome_pages_follow_the_same_scene_ownership_and_slot_reuse() {
        objc::rc::autoreleasepool(|| {
            let atlas = MetalAtlas::new(Device::system_default().expect("Metal device"));
            let mut params = match key(1) {
                AtlasKey::Glyph(params) => params,
                _ => unreachable!(),
            };
            params.is_emoji = true;
            let key = AtlasKey::Glyph(params);
            let tile = atlas
                .get_or_insert_with(&key, &mut || {
                    Ok(Some((
                        size(DevicePixels(16), DevicePixels(16)),
                        Cow::Owned(vec![7; 16 * 16 * 4]),
                    )))
                })
                .unwrap()
                .unwrap();
            let bounds = Bounds::new(
                point(ScaledPixels(0.), ScaledPixels(0.)),
                size(ScaledPixels(64.), ScaledPixels(64.)),
            );
            let mut scene = Scene::default();
            scene.insert_primitive(crate::PolychromeSprite {
                order: 0,
                pad: 0,
                grayscale: false,
                opacity: 1.,
                bounds,
                content_mask: ContentMask { bounds },
                corner_radii: Default::default(),
                tile: tile.clone(),
            });
            scene.finish();
            atlas.prepare_scene(&scene);
            assert_eq!(atlas.0.lock().tiles_by_key.get(&key), Some(&tile));
            atlas.prepare_scene(&Scene::default());
            assert!(atlas.0.lock().polychrome_textures.textures.iter().all(Option::is_none));
            let replacement = atlas
                .get_or_insert_with(&key, &mut || {
                    Ok(Some((
                        size(DevicePixels(16), DevicePixels(16)),
                        Cow::Owned(vec![9; 16 * 16 * 4]),
                    )))
                })
                .unwrap()
                .unwrap();
            assert_eq!(replacement.texture_id, tile.texture_id);
        });
    }

    #[test]
    fn playbit_historical_key_table_shrinks_after_scene_retirement() {
        objc::rc::autoreleasepool(|| {
            let atlas = MetalAtlas::new(Device::system_default().expect("Metal device"));
            for id in 0..8192 {
                insert(&atlas, id, 1, 7);
            }
            let previous = atlas.0.lock().tiles_by_key.capacity();
            assert!(previous > 2048);
            atlas.prepare_scene(&Scene::default());
            assert!(atlas.0.lock().tiles_by_key.capacity() < previous);
            assert!(atlas.0.lock().tiles_by_key.is_empty());
        });
    }

    fn pixel_key(id: usize) -> AtlasKey {
        AtlasKey::PixelImage(crate::RenderImageParams {
            image_id: crate::ImageId(id),
            frame_index: 0,
        })
    }

    fn insert_pixel(atlas: &MetalAtlas, id: usize, width: i32, height: i32) -> AtlasTile {
        atlas
            .get_or_insert_with(&pixel_key(id), &mut || {
                Ok(Some((
                    size(DevicePixels(width), DevicePixels(height)),
                    Cow::Owned(vec![7; (width * height * 4) as usize]),
                )))
            })
            .unwrap()
            .unwrap()
    }

    #[test]
    fn playbit_pixel_textures_keep_exact_extents_and_reject_invalid_sizes() {
        objc::rc::autoreleasepool(|| {
            let atlas = MetalAtlas::new(Device::system_default().expect("Metal device"));
            {
                let mut state = atlas.0.lock();
                for (width, height) in [(0, 1), (1, 0), (-1, 1), (1, -1), (16385, 1), (1, 16385)] {
                    assert!(
                        state
                            .allocate_pixels(size(DevicePixels(width), DevicePixels(height)))
                            .is_none()
                    );
                }
                assert!(state.polychrome_textures.textures.is_empty());
            }
            let first = insert_pixel(&atlas, 1, 4096, 2048);
            let tiny = insert_pixel(&atlas, 2, 1, 1);
            assert_ne!(first.texture_id, tiny.texture_id);
            for tile in [&first, &tiny] {
                let texture = atlas.metal_texture(tile.texture_id);
                assert_eq!(texture.width(), tile.bounds.size.width.0 as u64);
                assert_eq!(texture.height(), tile.bounds.size.height.0 as u64);
                assert_eq!(tile.bounds.origin, Point::default());
            }
            assert_eq!(insert_pixel(&atlas, 1, 4096, 2048), first);
            // Ordinary images still share their existing minimum-size atlas page.
            let ordinary_key =
                crate::RenderImageParams { image_id: crate::ImageId(3), frame_index: 0 };
            let ordinary = atlas
                .get_or_insert_with(&ordinary_key.into(), &mut || {
                    Ok(Some((size(DevicePixels(1), DevicePixels(1)), Cow::Owned(vec![7; 4]))))
                })
                .unwrap()
                .unwrap();
            let texture = atlas.metal_texture(ordinary.texture_id);
            assert_eq!((texture.width(), texture.height()), (256, 256));
            assert_ne!(ordinary.texture_id, first.texture_id);
            assert_ne!(ordinary.texture_id, tiny.texture_id);
        });
    }

    #[test]
    fn playbit_changing_wide_pixels_cannot_leave_pages_pinned_by_retained_tiny_pixels() {
        objc::rc::autoreleasepool(|| {
            let atlas = MetalAtlas::new(Device::system_default().expect("Metal device"));
            let mut retained = Vec::new();
            for frame in 0..128 {
                if frame > 0 {
                    atlas.remove(&pixel_key(frame - 1));
                }
                let changing = insert_pixel(&atlas, frame, 4096, 31);
                retained.push(insert_pixel(&atlas, 1024 + frame, 1, 1));
                let mut scene = Scene::default();
                let bounds = Bounds::new(
                    point(ScaledPixels(0.), ScaledPixels(0.)),
                    size(ScaledPixels(1.), ScaledPixels(1.)),
                );
                for tile in retained.iter().chain(std::iter::once(&changing)) {
                    scene.insert_primitive(crate::PolychromeSprite {
                        order: 0,
                        pad: 1,
                        grayscale: false,
                        opacity: 1.,
                        bounds,
                        content_mask: ContentMask { bounds },
                        corner_radii: Default::default(),
                        tile: tile.clone(),
                    });
                }
                scene.finish();
                // Unchanged CPU scene replay must preserve every current tile identity.
                let mut replayed = Scene::default();
                replayed.replay(0..scene.len(), &scene);
                replayed.finish();
                atlas.prepare_scene(&replayed);
                let state = atlas.0.lock();
                let textures: Vec<_> =
                    state.polychrome_textures.textures.iter().flatten().collect();
                let texel_bytes: u64 = textures
                    .iter()
                    .map(|t| t.metal_texture.width() * t.metal_texture.height() * 4)
                    .sum();
                assert_eq!(texel_bytes, 4096 * 31 * 4 + retained.len() as u64 * 4);
                assert!(texel_bytes <= 512 * 1024);
                assert_eq!(textures.len(), retained.len() + 1);
                assert!(state.polychrome_textures.textures.len() <= retained.len() + 2);
                assert_eq!(state.tiles_by_key.get(&pixel_key(frame)), Some(&changing));
            }
            atlas.prepare_scene(&Scene::default());
            assert!(atlas.0.lock().polychrome_textures.textures.iter().all(Option::is_none));
            assert!(atlas.0.lock().tiles_by_key.is_empty());
        });
    }

    #[test]
    fn playbit_encoded_metal_read_survives_retirement_and_page_index_reuse() {
        objc::rc::autoreleasepool(|| {
            let device = Device::system_default().expect("Metal device");
            let queue = device.new_command_queue();
            let atlas = MetalAtlas::new(device.clone());
            let original = insert(&atlas, 1, 16, 73);
            let texture = atlas.metal_texture(original.texture_id);
            let old_address = texture.as_ptr();
            let output = device.new_buffer(256, metal::MTLResourceOptions::StorageModeShared);
            let command = queue.new_command_buffer();
            let encoder = command.new_blit_command_encoder();
            encoder.copy_from_texture_to_buffer(
                &texture,
                0,
                0,
                metal::MTLOrigin {
                    x: original.bounds.origin.x.0 as u64,
                    y: original.bounds.origin.y.0 as u64,
                    z: 0,
                },
                metal::MTLSize { width: 1, height: 1, depth: 1 },
                &output,
                0,
                256,
                256,
                metal::MTLBlitOption::None,
            );
            encoder.end_encoding();
            drop(texture);
            // GPU work has not even been submitted. The command's strong
            // reference must preserve the old allocation through this cut.
            atlas.prepare_scene(&Scene::default());
            let replacement = insert(&atlas, 2, 16, 99);
            assert_eq!(replacement.texture_id, original.texture_id);
            assert_ne!(atlas.metal_texture(replacement.texture_id).as_ptr(), old_address);
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let complete = block::ConcreteBlock::new(move |_: &metal::CommandBufferRef| {
                send.send(()).expect("completion receiver alive");
            })
            .copy();
            command.add_completed_handler(&complete);
            command.commit();
            receive.recv_timeout(std::time::Duration::from_secs(10)).expect("GPU completion");
            assert_eq!(command.status(), metal::MTLCommandBufferStatus::Completed);
            // SAFETY: output owns at least one initialized byte, and the
            // completion callback establishes that the GPU write has finished.
            assert_eq!(unsafe { *output.contents().cast::<u8>() }, 73);
        });
    }
}
