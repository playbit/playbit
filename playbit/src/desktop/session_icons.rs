//! Display-only previews. One background job, with no file work on UI/IPC callbacks.
use crate::identifier::valid as valid_id;
use crate::package::Package;
use gpui::{Context, RenderImage};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
    sync::Arc,
};

type Preview = Result<Option<Arc<RenderImage>>, String>;
const ROW_PIXELS: u32 = 48;
pub(super) const MAX_RECENT_TILES: usize = 16;

#[derive(Default)]
pub struct Icons {
    entries: BTreeMap<String, Option<Preview>>,
    tiles: BTreeMap<String, Option<Preview>>,
    tile_pixels: u32,
    active: Option<(String, u32)>,
    account: String,
    generation: u64,
}
impl Icons {
    #[cfg(feature = "desktop-qualification")]
    pub(super) fn load_preview(&mut self, directory: &Path, sessions: &mut [super::SessionRow]) {
        let profile = super::profile_path();
        let builds = profile.join(crate::paths::BUILDS);
        std::fs::create_dir_all(&builds).expect("isolated preview build directory");
        for (index, session) in sessions.iter_mut().take(5).enumerate() {
            // Both preview sizes read the fixture through the normal package path.
            let prepare = || -> crate::package::Result<String> {
                let bytes = std::fs::read(directory.join(format!("{index}.png")))?;
                let package = Package::new(
                    "preview".into(),
                    format!("Preview {index}"),
                    b"\0asm\x01\0\0\0".to_vec(),
                    BTreeMap::from([(crate::package::codec::ICON.into(), bytes)]),
                )?;
                let version = package.id()?;
                package.write_to(&mut File::create(builds.join(&version))?)?;
                Ok(version)
            };
            let icon = prepare().map_err(|e| e.to_string()).and_then(|version| {
                session.resolved_build_id = version;
                load(&profile, &session.resolved_build_id, ROW_PIXELS)
            });
            self.entries.insert(session.resolved_build_id.clone(), Some(icon));
        }
    }

    pub(super) fn retry(&mut self, version: &str) {
        self.tiles.remove(version);
        if self.failed(version) {
            self.entries.remove(version);
        }
    }

    #[cfg(feature = "desktop-qualification")]
    pub(super) fn loading_preview(&mut self, sessions: &[super::SessionRow]) {
        self.entries.insert(sessions[1].resolved_build_id.clone(), Some(Ok(None)));
        for row in &sessions[3..5] {
            if matches!(row.build_state, crate::engine::builds::BuildState::Ready) {
                self.entries.insert(row.resolved_build_id.clone(), Some(Ok(None)));
            } else {
                self.entries.remove(&row.resolved_build_id);
            }
        }
    }

    fn reconcile(
        &mut self,
        sessions: &[super::SessionRow],
        imported: &[crate::store::ImportedApp],
    ) {
        let versions: BTreeSet<_> = sessions
            .iter()
            .take(256)
            .filter_map(|s| {
                valid_id(&s.resolved_build_id, 32).then_some(s.resolved_build_id.as_str())
            })
            .chain(
                imported
                    .iter()
                    .take(256)
                    .filter_map(|app| valid_id(&app.build_id, 32).then_some(app.build_id.as_str())),
            )
            .collect();
        // A name refresh does not make an immutable build's already-known artwork unknown.
        self.entries
            .retain(|version, outcome| versions.contains(version.as_str()) && outcome.is_some());
        self.tiles.retain(|version, _| versions.contains(version.as_str()));
        for row in sessions.iter().take(256) {
            if versions.contains(row.resolved_build_id.as_str())
                && matches!(row.build_state, crate::engine::builds::BuildState::Ready)
            {
                self.entries.entry(row.resolved_build_id.clone()).or_default();
            }
        }
        for app in imported.iter().take(256) {
            if versions.contains(app.build_id.as_str()) {
                self.entries.entry(app.build_id.clone()).or_default();
            }
        }
    }
    /// Queue larger previews only for displayed builds with known artwork.
    /// Their row icons remain available across zoom and display changes.
    pub(super) fn request_tiles(&mut self, versions: &BTreeSet<String>, side: u32) -> bool {
        assert!(versions.len() <= MAX_RECENT_TILES);
        let side = side.min(crate::package::artwork::MAX_TILE_PIXELS);
        if self.tile_pixels != side {
            self.tiles.clear();
            self.tile_pixels = side;
        }
        if side <= ROW_PIXELS {
            return false;
        }
        self.tiles.retain(|version, _| versions.contains(version));
        let retained = self.tiles.len();
        for version in versions {
            if matches!(self.entries.get(version), Some(Some(Ok(Some(_)))))
                && !self.tiles.contains_key(version)
            {
                self.tiles.insert(version.clone(), None);
            }
        }
        self.tiles.len() > retained
    }

    pub(super) fn tile(&self, version: &str) -> Option<Arc<RenderImage>> {
        match self.tiles.get(version) {
            Some(Some(Ok(Some(image)))) => Some(image.clone()),
            _ => self.get(version),
        }
    }

    fn next(&mut self) -> Option<(String, u32)> {
        if self.active.is_some() {
            return None;
        }
        for (entries, side) in [(&self.tiles, self.tile_pixels), (&self.entries, ROW_PIXELS)] {
            if let Some((version, _)) = entries.iter().find(|(_, value)| value.is_none()) {
                let job = (version.clone(), side);
                self.active = Some(job.clone());
                return Some(job);
            }
        }
        None
    }
    fn complete(&mut self, job: &(String, u32), preview: Preview) {
        assert_eq!(self.active.take().as_ref(), Some(job));
        let (version, side) = job;
        let entry = if *side == ROW_PIXELS {
            self.entries.get_mut(version)
        } else if *side == self.tile_pixels {
            self.tiles.get_mut(version)
        } else {
            None
        };
        if let Some(entry) = entry {
            *entry = Some(preview.map_err(|mut error| {
                let mut end = error.len().min(256);
                while !error.is_char_boundary(end) {
                    end -= 1;
                }
                error.truncate(end);
                error
            }));
        }
    }
    pub(super) fn known(&self, version: &str) -> bool {
        matches!(self.entries.get(version), Some(Some(_)))
    }
    pub fn get(&self, version: &str) -> Option<Arc<RenderImage>> {
        self.entries.get(version)?.as_ref()?.as_ref().ok()?.clone()
    }
    pub fn failures(&self) -> usize {
        self.entries.values().filter(|v| matches!(v, Some(Err(_)))).count()
    }
    pub fn failed(&self, version: &str) -> bool {
        matches!(self.entries.get(version), Some(Some(Err(_))))
    }
}

impl super::Shell {
    pub(super) fn retry_session_info(&mut self, session: &str, cx: &mut Context<Self>) {
        let Some(row) = self.sessions.iter().find(|row| row.id == session) else { return };
        if let Err(error) =
            self.send.send(super::Request::RetrySessionInfo { session: session.into() })
        {
            self.error = error.into();
        } else {
            self.session_icons.retry(&row.resolved_build_id);
            self.refresh_session_icons(cx);
        }
        cx.notify();
    }

    pub fn refresh_session_icons(&mut self, cx: &mut Context<Self>) {
        if self.session_icons.account != self.account_id {
            self.session_icons.entries.clear();
            self.session_icons.tiles.clear();
            self.session_icons.account.clone_from(&self.account_id);
            self.session_icons.generation += 1;
        }
        self.session_icons.reconcile(&self.sessions, self.applications.imported_apps());
        // Retired controls must not keep old pixels alive behind the cache cap.
        for (id, control) in &self.navigation_controls {
            let version = self
                .sessions
                .iter()
                .find(|session| session.id == *id)
                .map(|session| session.resolved_build_id.as_str())
                .unwrap_or("");
            let icon = self.session_icons.get(version);
            let failed = self.session_icons.failed(version);
            let loading = !self.session_icons.known(version);
            control.update(cx, |control, cx| {
                control.set_app_icon(icon, failed, cx);
                control.set_session_icon_loading(loading, cx);
            });
        }
        self.load_next_session_icon(cx);
    }

    pub(super) fn load_next_session_icon(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.session_icons.next() else {
            return;
        };
        let generation = self.session_icons.generation;
        let profile = super::profile_path();
        let (job_version, side) = request.clone();
        let job = cx.background_executor().spawn(async move {
            let result = load(&profile, &job_version, side);
            if let Err(error) = &result {
                crate::diagnostic!("App icon {job_version}: {error}");
            }
            result
        });
        cx.spawn(async move |shell, cx| {
            let preview = job.await;
            let _ = shell.update(cx, |shell, cx| {
                let _timing = crate::trace::Span::new("ui.session_icon.apply");
                if shell.session_icons.generation == generation {
                    shell.session_icons.complete(&request, preview);
                } else {
                    // Keep the single decode admission occupied until its old job finishes.
                    shell.session_icons.active = None;
                }
                if side == ROW_PIXELS {
                    shell.refresh_session_icons(cx);
                } else {
                    shell.load_next_session_icon(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn load(profile: &Path, version: &str, side: u32) -> Preview {
    let _timing = crate::trace::Span::new("ui.session_icon.load");
    if !valid_id(version, 32) {
        return Err("invalid app identity".into());
    }
    let bytes = {
        let _timing = crate::trace::Span::new("ui.session_icon.read");
        let mut file = File::open(profile.join(crate::paths::BUILDS).join(version))
            .map_err(|e| e.to_string())?;
        Package::read_display_icon(&mut file).map_err(|e| e.to_string())?
    };
    bytes.map(|bytes| decode(&bytes, side)).transpose()
}

/// Review artwork occupies a 48-point slot on a Retina display.
pub(super) fn decode_review(bytes: &[u8]) -> Result<Arc<RenderImage>, String> {
    decode(bytes, 96)
}

fn decode(bytes: &[u8], side: u32) -> Result<Arc<RenderImage>, String> {
    let _timing = crate::trace::Span::new("ui.session_icon.decode");
    let mut rgba = crate::package::artwork::decode(bytes, side)?;
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([image::Frame::new(rgba)])))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::io::Cursor;
    const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\">\
        <rect width=\"24\" height=\"24\" fill=\"#ff0000\"/></svg>";
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, image::ImageFormat::Png).unwrap();
        output.into_inner()
    }
    #[test]
    fn raster_and_vector_share_bounded_bgra_and_preserve_alpha() {
        for input in [SVG.to_vec(), png(48, 48)] {
            let preview = decode(&input, ROW_PIXELS).unwrap();
            assert_eq!(preview.frame_count(), 1);
            let bytes = preview.as_bytes(0).unwrap();
            assert_eq!(bytes.len(), 48 * 48 * 4);
            assert!(bytes.as_chunks::<4>().0.iter().all(|pixel| *pixel == [0, 0, 255, 255]));
        }
        assert_eq!(decode_review(SVG).unwrap().as_bytes(0).unwrap().len(), 96 * 96 * 4);
        let transparent = decode(
            b"<svg width=\"24\" height=\"24\"><rect width=\"24\" height=\"24\" \
              fill=\"red\" opacity=\"0.5\"/></svg>",
            ROW_PIXELS,
        )
        .unwrap();
        assert_eq!(&transparent.as_bytes(0).unwrap()[0..4], &[0, 0, 255, 128]);
        let wide = decode(&png(48, 24), ROW_PIXELS).unwrap();
        assert_eq!(&wide.as_bytes(0).unwrap()[0..4], &[0, 0, 0, 0]);
        assert!(decode(include_bytes!("../../assets/check-on.svg"), ROW_PIXELS).is_ok());
    }
    #[test]
    fn reject_unbounded_or_external_artwork() {
        assert!(decode(&png(1025, 1), ROW_PIXELS).is_err());
        assert!(decode(&vec![0; crate::package::codec::MAX_ICON_BYTES + 1], ROW_PIXELS).is_err());
        for body in [
            "<image href='/etc/passwd'/>",
            "<use href='#x'/>",
            "<filter/>",
            "<text>Hello</text>",
            "<rect fill='url(file:///etc/passwd)'/>",
            "<linearGradient href='#x'/>",
        ] {
            assert!(
                decode(format!("<svg width='24' height='24'>{body}</svg>").as_bytes(), ROW_PIXELS)
                    .is_err()
            );
        }
        assert!(
            decode(format!("<svg>{}</svg>", "<g/>".repeat(513)).as_bytes(), ROW_PIXELS).is_err()
        );
        assert!(decode(b"<!DOCTYPE svg [<!ENTITY x 'x'>]><svg>&x;</svg>", ROW_PIXELS).is_err());
        assert!(load(Path::new("/tmp"), "../file", ROW_PIXELS).is_err());
    }
    #[test]
    fn jpeg_and_local_gradients_render_without_external_resources() {
        let mut jpeg = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(24, 24, image::Rgb([255, 0, 0]))
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let preview = decode(jpeg.get_ref(), ROW_PIXELS).unwrap();
        assert_eq!(preview.as_bytes(0).unwrap().len(), 48 * 48 * 4);
        assert!(preview.as_bytes(0).unwrap()[2] > 240);
        let preview = decode(
            b"<svg width=\"24\" height=\"24\"><defs><linearGradient id=\"color\">\
              <stop stop-color=\"red\"/><stop offset=\"1\" stop-color=\"blue\"/>\
              </linearGradient></defs><rect width=\"24\" height=\"24\" \
              fill=\"url(#color)\"/></svg>",
            ROW_PIXELS,
        )
        .unwrap();
        let bytes = preview.as_bytes(0).unwrap();
        assert!(bytes[2] > bytes[0]);
        assert!(bytes[47 * 4] > bytes[47 * 4 + 2]);
    }
    #[test]
    fn cache_bounds_single_admission_and_retires_stale_results() {
        let status = |n| {
            (0..n)
                .map(|i| super::super::SessionRow {
                    reference: String::new(),
                    id: crate::identifier::encode(&{
                        let mut bytes = [0; 32];
                        bytes[16..].copy_from_slice(&(i as u128).to_be_bytes());
                        bytes
                    }),
                    resolved_build_id: crate::identifier::encode(&{
                        let mut bytes = [0; 32];
                        bytes[16..].copy_from_slice(&(i as u128).to_be_bytes());
                        bytes
                    }),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some("App".into()),
                })
                .collect::<Vec<_>>()
        };
        let mut icons = Icons::default();
        icons.reconcile(&status(300), &[]);
        assert_eq!(icons.entries.len(), 256);
        let first = icons.next().unwrap();
        assert!(icons.next().is_none());
        icons.reconcile(&status(0), &[]);
        icons.complete(&first, Ok(Some(decode(SVG, ROW_PIXELS).unwrap())));
        assert!(icons.entries.is_empty());
        icons.reconcile(&status(4), &[]);
        let first = icons.next().unwrap();
        icons.complete(&first, Err("invalid artwork".into()));
        icons.reconcile(&status(4), &[]);
        assert_eq!(icons.failures(), 1);
        assert_ne!(
            icons.next().unwrap(),
            first,
            "failed immutable artwork is not retried on status changes"
        );
    }
    #[test]
    fn shared_builds_share_pixels_and_changed_builds_replace_them() {
        let version = crate::identifier::encode(&[0x11; 32]);
        let changed = crate::identifier::encode(&[0x22; 32]);
        let status = |version: &str| {
            (0..128)
                .map(|i| super::super::SessionRow {
                    reference: String::new(),
                    id: crate::identifier::encode(&{
                        let mut bytes = [0; 32];
                        bytes[16..].copy_from_slice(&(i as u128).to_be_bytes());
                        bytes
                    }),
                    resolved_build_id: version.into(),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some("Same name".into()),
                })
                .collect::<Vec<_>>()
        };
        let mut icons = Icons::default();
        icons.reconcile(&status(&version), &[]);
        assert_eq!(icons.entries.len(), 1);
        let active = icons.next().unwrap();
        let preview = decode(SVG, ROW_PIXELS).unwrap();
        icons.complete(&active, Ok(Some(preview.clone())));
        assert!(Arc::ptr_eq(&preview, &icons.get(&version).unwrap()));
        let mut refreshing = status(&version);
        for row in &mut refreshing {
            row.build_state = crate::engine::builds::BuildState::Checking;
        }
        icons.reconcile(&refreshing, &[]);
        assert!(
            Arc::ptr_eq(&preview, &icons.get(&version).unwrap()),
            "known artwork survives a metadata refresh"
        );
        assert!(icons.next().is_none());
        icons.reconcile(&status(&changed), &[]);
        assert!(icons.get(&version).is_none());
        assert!(icons.get(&changed).is_none());
        assert_eq!(icons.next().unwrap(), (changed, ROW_PIXELS));
    }

    #[test]
    fn recent_tiles_keep_source_detail_and_retire_stale_sizes() {
        // The row averages these stripes to gray. The Retina tile resolves each one.
        let stripes = image::RgbImage::from_fn(96, 96, |x, _| {
            image::Rgb([if x % 2 == 0 { 0 } else { 255 }; 3])
        });
        let mut png = Cursor::new(Vec::new());
        stripes.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let version = crate::identifier::encode(&[1; 32]);
        let versions = BTreeSet::from([version.clone()]);
        let row = decode(png.get_ref(), ROW_PIXELS).unwrap();
        let mut icons = Icons::default();
        icons.entries.insert(version.clone(), Some(Ok(Some(row.clone()))));
        assert!(!icons.request_tiles(&versions, ROW_PIXELS));
        assert!(icons.request_tiles(&versions, 96));
        let request = icons.next().unwrap();
        assert_eq!(request, (version.clone(), 96));
        let tile = decode(png.get_ref(), request.1).unwrap();
        assert_eq!(tile.as_bytes(0).unwrap()[..8], [0, 0, 0, 255, 255, 255, 255, 255]);
        assert_eq!(row.as_bytes(0).unwrap()[..4], [128, 128, 128, 255]);
        icons.complete(&request, Ok(Some(tile.clone())));
        assert!(Arc::ptr_eq(&tile, &icons.tile(&version).unwrap()));
        assert!(Arc::ptr_eq(&row, &icons.get(&version).unwrap()));
        assert!(!icons.request_tiles(&versions, 96));

        assert!(icons.request_tiles(&versions, 120));
        let stale = icons.next().unwrap();
        assert!(icons.request_tiles(&versions, 192));
        assert!(icons.next().is_none());
        icons.complete(&stale, Ok(Some(decode(SVG, 120).unwrap())));
        assert!(Arc::ptr_eq(&row, &icons.tile(&version).unwrap()));
        let retired = icons.next().unwrap();
        assert_eq!(retired, (version.clone(), 192));
        assert!(!icons.request_tiles(&BTreeSet::new(), 192));
        icons.complete(&retired, Ok(Some(decode(SVG, 192).unwrap())));
        assert!(icons.tiles.is_empty());
        assert!(icons.next().is_none());
    }
    #[test]
    #[ignore = "reference-device icon benchmark"]
    fn benchmark_icon_decode() {
        let inputs = [SVG.to_vec(), png(24, 24), png(256, 256), png(512, 512)];
        for input in inputs {
            let mut times = Vec::new();
            for _ in 0..1000 {
                let start = std::time::Instant::now();
                std::hint::black_box(decode(&input, ROW_PIXELS).unwrap());
                times.push(start.elapsed().as_secs_f64() * 1000.);
            }
            times.sort_by(f64::total_cmp);
            crate::diagnostic!(
                "{}-byte icon decode, 1000 samples: p50 {:.3} ms, p99 {:.3}, max {:.3}",
                input.len(),
                times[499],
                times[989],
                times[999]
            );
            assert!(times[989] < 10.);
        }
    }

    #[test]
    #[ignore = "reference-device icon I/O benchmark"]
    fn benchmark_icon_load() {
        let profile = std::env::temp_dir().join(format!(
            "pb-icon-bench-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(profile.join(crate::paths::BUILDS)).unwrap();
        let mut versions = Vec::new();
        for (index, bytes) in
            [SVG.to_vec(), png(24, 24), png(256, 256), png(512, 512)].into_iter().enumerate()
        {
            let mut wasm = b"\0asm\x01\0\0\0".to_vec();
            wasm.resize(1024 * 1024, 0);
            let app = Package::new(
                "test".into(),
                format!("App {index}"),
                wasm,
                BTreeMap::from([
                    (crate::package::codec::ICON.into(), bytes),
                    ("unrelated".into(), vec![0; 1024 * 1024]),
                ]),
            )
            .unwrap();

            let version = app.id().unwrap();
            std::fs::write(
                profile.join(crate::paths::BUILDS).join(&version),
                app.to_bytes().unwrap(),
            )
            .unwrap();
            versions.push(version);
        }
        let mut times = Vec::new();
        for _ in 0..1000 {
            let start = std::time::Instant::now();
            for version in &versions {
                std::hint::black_box(load(&profile, version, ROW_PIXELS).unwrap());
            }
            times.push(start.elapsed().as_secs_f64() * 1000.);
        }
        times.sort_by(f64::total_cmp);
        crate::diagnostic!(
            "four headers/icons; each 1 MiB Wasm + 1 MiB unrelated resource; 1000 warm-file samples: p50 {:.3} ms, p99 {:.3}, max {:.3}",
            times[499],
            times[989],
            times[999]
        );
        std::fs::remove_dir_all(profile).unwrap();
        assert!(times[989] < 10.);
    }
}
