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
#[derive(Default)]
pub struct Icons {
    entries: BTreeMap<String, Option<Preview>>,
    active: Option<String>,
    account: String,
    generation: u64,
}
impl Icons {
    #[cfg(feature = "desktop-qualification")]
    pub(super) fn load_preview(&mut self, directory: &Path, sessions: &[super::SessionRow]) {
        for (index, session) in sessions.iter().take(5).enumerate() {
            let icon = std::fs::read(directory.join(format!("{index}.png")))
                .map_err(|e| e.to_string())
                .and_then(|bytes| decode(&bytes));
            self.entries.insert(session.resolved_build_id.clone(), Some(icon.map(Some)));
        }
    }

    pub(super) fn retry(&mut self, version: &str) {
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
    fn next(&mut self) -> Option<String> {
        if self.active.is_some() {
            return None;
        }
        let version = self.entries.iter().find(|(_, value)| value.is_none())?.0.clone();
        self.active = Some(version.clone());
        Some(version)
    }
    fn complete(&mut self, version: &str, preview: Preview) {
        assert_eq!(self.active.as_deref(), Some(version));
        self.active = None;
        if let Some(entry) = self.entries.get_mut(version) {
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
        let Some(version) = self.session_icons.next() else {
            return;
        };
        let generation = self.session_icons.generation;
        let profile = super::profile_path();
        let job_version = version.clone();
        let job = cx.background_executor().spawn(async move {
            let result = load(&profile, &job_version);
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
                    shell.session_icons.complete(&version, preview);
                } else {
                    // Keep the single decode admission occupied until its old job finishes.
                    shell.session_icons.active = None;
                }
                shell.refresh_session_icons(cx);
                cx.notify();
            });
        })
        .detach();
    }
}

fn load(profile: &Path, version: &str) -> Preview {
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
    bytes.map(|bytes| decode(&bytes)).transpose()
}

fn decode(bytes: &[u8]) -> Result<Arc<RenderImage>, String> {
    let _timing = crate::trace::Span::new("ui.session_icon.decode");
    let mut rgba = crate::package::artwork::decode(bytes, 48)?;
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
    const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24"><rect width="24" height="24" fill="#ff0000"/></svg>"##;
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, image::ImageFormat::Png).unwrap();
        output.into_inner()
    }
    #[test]
    fn raster_and_vector_share_bounded_bgra_and_preserve_alpha() {
        for input in [SVG.to_vec(), png(48, 48)] {
            let preview = decode(&input).unwrap();
            assert_eq!(preview.frame_count(), 1);
            let bytes = preview.as_bytes(0).unwrap();
            assert_eq!(bytes.len(), 48 * 48 * 4);
            assert!(bytes.as_chunks::<4>().0.iter().all(|pixel| *pixel == [0, 0, 255, 255]));
        }
        let transparent = decode(br#"<svg width="24" height="24"><rect width="24" height="24" fill="red" opacity="0.5"/></svg>"#).unwrap();
        assert_eq!(&transparent.as_bytes(0).unwrap()[0..4], &[0, 0, 255, 128]);
        let wide = decode(&png(48, 24)).unwrap();
        assert_eq!(&wide.as_bytes(0).unwrap()[0..4], &[0, 0, 0, 0]);
        assert!(decode(include_bytes!("../../assets/check-on.svg")).is_ok());
    }
    #[test]
    fn reject_unbounded_or_external_artwork() {
        assert!(decode(&png(1025, 1)).is_err());
        assert!(decode(&vec![0; crate::package::codec::MAX_ICON_BYTES + 1]).is_err());
        for body in [
            "<image href='/etc/passwd'/>",
            "<use href='#x'/>",
            "<filter/>",
            "<text>Hello</text>",
            "<rect fill='url(file:///etc/passwd)'/>",
            "<linearGradient href='#x'/>",
        ] {
            assert!(
                decode(format!("<svg width='24' height='24'>{body}</svg>").as_bytes()).is_err()
            );
        }
        assert!(decode(format!("<svg>{}</svg>", "<g/>".repeat(513)).as_bytes()).is_err());
        assert!(decode(b"<!DOCTYPE svg [<!ENTITY x 'x'>]><svg>&x;</svg>").is_err());
        assert!(load(Path::new("/tmp"), "../file").is_err());
    }
    #[test]
    fn jpeg_and_local_gradients_render_without_external_resources() {
        let mut jpeg = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(24, 24, image::Rgb([255, 0, 0]))
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let preview = decode(jpeg.get_ref()).unwrap();
        assert_eq!(preview.as_bytes(0).unwrap().len(), 48 * 48 * 4);
        assert!(preview.as_bytes(0).unwrap()[2] > 240);
        let preview = decode(br##"<svg width="24" height="24"><defs><linearGradient id="color"><stop stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient></defs><rect width="24" height="24" fill="url(#color)"/></svg>"##).unwrap();
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
        icons.complete(&first, Ok(Some(decode(SVG).unwrap())));
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
        let preview = decode(SVG).unwrap();
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
        assert_eq!(icons.next().unwrap(), changed);
    }
    #[test]
    #[ignore = "reference-device icon benchmark"]
    fn benchmark_icon_decode() {
        let inputs = [SVG.to_vec(), png(24, 24), png(256, 256), png(512, 512)];
        for input in inputs {
            let mut times = Vec::new();
            for _ in 0..1000 {
                let start = std::time::Instant::now();
                std::hint::black_box(decode(&input).unwrap());
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
                std::hint::black_box(load(&profile, version).unwrap());
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
