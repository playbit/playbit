//! Only the visible overview requests package metadata and published release notes.
use super::*;
use crate::engine::reply::DeveloperReply;
use crate::network::ap::operation::AppBuild;
use std::{net::Shutdown, os::unix::net::UnixStream, path::Path, sync::Mutex};
use unicode_segmentation::UnicodeSegmentation;

mod watch;

const MAX_NOTES: usize = 32;
const NOTE_PAGE_BYTES: usize = 1024;
const NOTE_PAGE_LINES: usize = 12;

#[derive(Clone)]
pub(super) struct ReleaseNote {
    pub(super) build_id: SharedString,
    text: SharedString,
    pub(super) published_at: u64,
    page_ends: Arc<[usize]>,
    pub(super) page: usize,
}

impl ReleaseNote {
    pub(super) fn new(build_id: String, text: String, published_at: u64) -> Self {
        // Compute the page boundaries on the history worker. In the pane each release
        // contributes at most one small block to native text shaping and accessibility.
        let mut page_ends = Vec::new();
        let mut start = 0;
        let mut lines = 0;
        for (at, grapheme) in text.grapheme_indices(true) {
            let end = at + grapheme.len();
            if end - start > NOTE_PAGE_BYTES && at > start {
                page_ends.push(at);
                start = at;
                lines = 0;
            }
            // A single grapheme can exceed the entire page budget.
            while end - start > NOTE_PAGE_BYTES {
                start = text.floor_char_boundary(start + NOTE_PAGE_BYTES);
                page_ends.push(start);
            }
            if grapheme.ends_with('\n') {
                lines += 1;
                if lines == NOTE_PAGE_LINES {
                    page_ends.push(end);
                    start = end;
                    lines = 0;
                }
            }
        }
        if start < text.len() {
            page_ends.push(text.len());
        }
        Self {
            build_id: build_id.into(),
            text: text.into(),
            published_at,
            page_ends: page_ends.into(),
            page: 0,
        }
    }

    pub(super) fn page_count(&self) -> usize {
        self.page_ends.len()
    }

    pub(super) fn page_text(&self) -> SharedString {
        if self.page_count() <= 1 {
            return self.text.clone();
        }
        let start = self.page.checked_sub(1).map_or(0, |i| self.page_ends[i]);
        SharedString::new(&self.text[start..self.page_ends[self.page]])
    }
}

#[derive(Clone, Default)]
pub(super) struct History {
    pub(super) updated: Option<u64>,
    pub(super) notes: Vec<ReleaseNote>,
}

impl History {
    fn from_builds(selected_build: &str, builds: Vec<AppBuild>) -> Self {
        let mut page = Self::default();
        for build in builds {
            if build.build_id == selected_build {
                page.updated = Some(build.published_at);
            }
            if !build.notes.trim().is_empty() {
                page.notes.push(ReleaseNote::new(build.build_id, build.notes, build.published_at));
            }
        }
        page
    }

    fn merge(&mut self, page: Self) {
        self.updated = page.updated.or(self.updated);
        self.notes.extend(page.notes);
        // AP pages follow build IDs. Each page competes with the newest notes seen so far.
        self.notes.sort_unstable_by(|a, b| {
            b.published_at.cmp(&a.published_at).then_with(|| a.build_id.cmp(&b.build_id))
        });
        self.notes.truncate(MAX_NOTES);
    }
}

#[derive(Default)]
struct Read {
    retired: bool,
    socket: Option<UnixStream>,
}

impl Read {
    fn retire(active: &Mutex<Self>) {
        let mut read = active.lock().expect("release history socket mutex is not poisoned");
        read.retired = true;
        if let Some(socket) = read.socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct LocalBuild {
    imported: bool,
    ready: bool,
}

enum PageError {
    Retry(String),
    RecheckVisibility(String),
}

impl PageError {
    fn from_engine(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        let message = error.to_string();
        // ListAppBuilds checks visibility on every AP page. The engine forwards this
        // rejection verbatim when the publication is deleted or no longer visible.
        if error.is::<ipc::RejectedReply>() && message == "app is unavailable" {
            Self::RecheckVisibility(message)
        } else {
            Self::Retry(message)
        }
    }
}

#[derive(Default)]
pub(super) struct Details {
    // The latest desired selection can change while a retired reader is unwinding.
    target: Option<(Id, AppIdentity, String)>,
    active: Option<Arc<Mutex<Read>>>,
    watch: Option<watch::Subscription>,
    watch_owner: Option<(RequestSender, u64)>,
    pub(super) watch_error: Option<String>,
    #[cfg(any(test, feature = "desktop-qualification"))]
    fixture: bool,
    size_read: Option<Rc<()>>,
    size_checked: Option<LocalBuild>,
    pub(super) size: Option<u64>,
    pub(super) history: Option<History>,
    pub(super) history_error: Option<String>,
    next: Option<String>,
    // Coalesce pushes while the current bounded scan finishes. Keep the committed
    // notes visible until every history page in the replacement has been read.
    invalidated: bool,
    replacement: Option<History>,
}

impl Details {
    pub(super) fn cancel(&self) {
        if let Some(active) = &self.active {
            Read::retire(active);
        }
        if let Some(watch) = &self.watch {
            Read::retire(&watch.read);
        }
    }

    fn select(&mut self, account: Id, app: Option<(&AppIdentity, &str)>) {
        let current = match (&self.target, app) {
            (Some((owner, identity, build)), Some((app, version))) => {
                *owner == account && identity == app && build == version
            }
            (None, None) => true,
            _ => false,
        };
        if !current {
            self.cancel();
            self.target = app.map(|(identity, build)| (account, identity.clone(), build.into()));
            self.size = None;
            self.size_checked = None;
            self.history = None;
            self.history_error = None;
            self.next = None;
            self.invalidated = false;
            self.replacement = None;
            self.watch_error = None;
            #[cfg(any(test, feature = "desktop-qualification"))]
            {
                self.fixture = false;
            }
        }
    }

    fn request_size(&mut self, local: LocalBuild) -> Option<Rc<()>> {
        // Forget lost availability even during a stat. A later import or Ready
        // notification then admits one fresh read after the current read returns.
        if let Some(checked) = &mut self.size_checked {
            checked.imported &= local.imported;
            checked.ready &= local.ready;
        }
        if self.size_read.is_some() || self.size_checked == Some(local) {
            return None;
        }
        let read = Rc::new(());
        self.size_read = Some(read.clone());
        self.size_checked = Some(local);
        Some(read)
    }

    fn finish_size(&mut self, read: &Rc<()>, size: Option<u64>) {
        if self.size_read.as_ref().is_some_and(|active| Rc::ptr_eq(active, read)) {
            self.size_read = None;
            if self.size_checked.is_some() {
                self.size = size;
            }
        }
    }

    pub(super) fn needs_history_page(&self) -> bool {
        self.invalidated
            || self.history_error.is_none()
                && (self.history.is_none() || self.next.is_some() || self.replacement.is_some())
    }

    fn retry_history(&mut self) -> bool {
        let watch = self.watch_error.take().is_some();
        if watch && let Some(current) = &self.watch {
            Read::retire(&current.read);
        }
        self.history_error.take().is_some() || watch
    }

    fn prepare_history_page(&mut self) {
        if self.invalidated && (self.next.is_none() || self.history_error.is_some()) {
            self.invalidated = false;
            self.history_error = None;
            self.next = None;
            self.replacement = self.history.as_ref().map(|_| History::default());
        }
    }

    fn finish(
        &mut self,
        read: &Arc<Mutex<Read>>,
        result: Result<(History, Option<String>), PageError>,
    ) {
        if self.active.as_ref().is_some_and(|active| Arc::ptr_eq(active, read)) {
            self.active = None;
            if !read.lock().expect("release history socket mutex is not poisoned").retired {
                match result {
                    Ok((page, next)) => {
                        if let Some(replacement) = &mut self.replacement {
                            replacement.merge(page);
                            if next.is_none() {
                                if let Some(old) = &self.history {
                                    for note in &mut replacement.notes {
                                        if let Some(previous) = old
                                            .notes
                                            .iter()
                                            .find(|n| n.page != 0 && n.build_id == note.build_id)
                                        {
                                            note.page = previous
                                                .page
                                                .min(note.page_count().saturating_sub(1));
                                        }
                                    }
                                }
                                self.history = self.replacement.take();
                            }
                        } else {
                            self.history.get_or_insert_with(History::default).merge(page);
                        }
                        self.next = next;
                    }
                    Err(PageError::Retry(error)) => self.history_error = Some(error),
                    Err(PageError::RecheckVisibility(error)) => {
                        self.history = None;
                        self.next = None;
                        self.replacement = None;
                        self.invalidated = false;
                        self.history_error = Some(error);
                    }
                }
            }
        }
    }

    #[cfg(any(test, feature = "desktop-qualification"))]
    pub(super) fn preview(
        account: Id,
        app: &AppIdentity,
        build: &str,
        size: Option<u64>,
        history: History,
    ) -> Self {
        Self {
            target: Some((account, app.clone(), build.into())),
            active: None,
            watch: None,
            watch_owner: None,
            watch_error: None,
            fixture: true,
            size_read: None,
            size_checked: None,
            size,
            history: Some(history),
            history_error: None,
            next: None,
            invalidated: false,
            replacement: None,
        }
    }
}

impl Drop for Details {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Shell {
    pub(in crate::desktop) fn sync_application_details(
        &mut self,
        app: Option<(&AppIdentity, &str)>,
        cx: &mut Context<Self>,
    ) {
        self.applications.details.select(self.account.id, app);
        if let Some((_, build)) = app
            && self.applications.details.size.is_none()
        {
            self.refresh_package_size(build, cx);
        }
        if self.applications.creating {
            return;
        }
        self.sync_app_history_watch(cx);
        let details = &mut self.applications.details;
        if details.active.is_some() || !details.needs_history_page() {
            return;
        }
        let Some((account, identity, build)) = details.target.clone() else { return };
        details.prepare_history_page();
        let after = details.next.clone();
        let read = Arc::new(Mutex::new(Read::default()));
        details.active = Some(read.clone());
        let sender = self.send.clone();
        let gate = sender.gate();
        let epoch = gate.epoch();
        let profile = profile_path();
        let history_job = cx.background_executor().spawn({
            let read = read.clone();
            let gate = gate.clone();
            async move {
                read_history_page(&profile, &identity, after, &read, &gate, epoch)
                    .map(|page| (History::from_builds(&build, page.builds), page.next))
                    .map_err(PageError::from_engine)
            }
        });
        cx.spawn(async move |host, cx| {
            let history = history_job.await;
            let _ = host.update(cx, |host, cx| {
                let history = if host.send.same_worker(&sender)
                    && gate.accepts(epoch)
                    && host.account.id == account
                    && !host.signing_out
                {
                    history
                } else {
                    Err(PageError::RecheckVisibility(worker_queue::RESTARTING.into()))
                };
                host.applications.details.finish(&read, history);
                // Rendering now admits the latest desired selection, if any.
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_package_size(&mut self, build: &str, cx: &mut Context<Self>) {
        let local = LocalBuild {
            imported: self.applications.imported.iter().any(|app| app.build_id == build),
            ready: self.sessions.iter().any(|row| {
                row.resolved_build_id == build
                    && matches!(row.build_state, crate::engine::builds::BuildState::Ready)
            }),
        };
        let Some(read) = self.applications.details.request_size(local) else { return };
        let account = self.account.id;
        let sender = self.send.clone();
        let profile = profile_path();
        let build = build.to_owned();
        let job = cx.background_executor().spawn(async move { package_size(&profile, &build) });
        cx.spawn(async move |host, cx| {
            let size = job.await;
            let _ = host.update(cx, |host, cx| {
                let current = host.account.id == account
                    && host.send.same_worker(&sender)
                    && !host.signing_out;
                host.applications.details.finish_size(&read, if current { size } else { None });
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn retry_application_details(&mut self, cx: &mut Context<Self>) {
        if self.applications.details.retry_history() {
            cx.notify();
        }
    }
}

fn package_size(profile: &Path, build: &str) -> Option<u64> {
    if !crate::identifier::valid(build, 32) {
        return None;
    }
    let file = std::fs::symlink_metadata(profile.join(crate::paths::BUILDS).join(build)).ok()?;
    (file.is_file() && file.len() <= crate::package::MAX_BUNDLE_BYTES as u64).then_some(file.len())
}

fn read_history_page(
    profile: &Path,
    identity: &AppIdentity,
    after: Option<String>,
    read: &Mutex<Read>,
    gate: &worker_queue::Gate,
    epoch: u64,
) -> crate::engine::Result<crate::engine::reply::AppBuildPage> {
    let mut socket = connect_read(profile, read, gate, epoch)?;
    let request = Request::ListAppBuilds { app: identity.clone(), after };
    let response = ipc::call_stream_archive(&mut socket, &request);
    read.lock().expect("release history socket mutex is not poisoned").socket = None;
    let Reply::Developer(DeveloperReply::AppBuilds(page)) = response?.into_owned()? else {
        return Err("Unexpected app release history reply".into());
    };
    Ok(page)
}

fn connect_read(
    profile: &Path,
    read: &Mutex<Read>,
    gate: &worker_queue::Gate,
    epoch: u64,
) -> crate::engine::Result<UnixStream> {
    if read.lock().expect("release history socket mutex is not poisoned").retired
        || !gate.accepts(epoch)
    {
        return Err(worker_queue::RETIRED.into());
    }
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    socket.connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?)?;
    let socket: UnixStream = socket.into();
    socket.set_nonblocking(false)?;
    {
        let mut active = read.lock().expect("release history socket mutex is not poisoned");
        if active.retired || !gate.accepts(epoch) {
            return Err(worker_queue::RETIRED.into());
        }
        active.socket = Some(socket.try_clone()?);
    }
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[gpui::test]
    fn ready_notification_updates_missing_size_and_preserves_loaded_history(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::engine::builds::BuildState;
        let (host, _, _requests) = test_support::host(cx);
        let bytes = *Id::generate().expect("unique build fixture").as_bytes();
        let build = crate::identifier::encode(blake3::hash(&bytes).as_bytes());
        let builds = profile_path().join(crate::paths::BUILDS);
        std::fs::create_dir_all(&builds).expect("test package cache");
        let path = tempfile::TempPath::try_from_path(builds.join(&build)).expect("fixture path");
        let app = host.update(cx, |host, cx| {
            let app = AppIdentity { publisher: host.account.id, app_id: "sketch".into() };
            host.applications.scope_refreshing = true;
            host.apply_imported_app_catalog(vec![crate::store::ImportedApp {
                app: app.clone(),
                build_id: build.clone(),
                name: "Sketch".into(),
            }]);
            host.sessions.push(SessionRow {
                id: crate::identifier::encode(&[9; 32]),
                reference: String::new(),
                resolved_build_id: build.clone(),
                app_name: Some("Sketch".into()),
                build_state: BuildState::Downloading,
            });
            assert!(host.select_application_target(Target::Application(app.clone()), cx));
            host.applications.details = Details::preview(
                host.account.id,
                &app,
                &build,
                None,
                History { updated: Some(42), notes: Vec::new() },
            );
            host.sync_application_details(Some((&app, &build)), cx);
            app
        });
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.applications.details.size_read.is_none(), "initial stat finished");
            assert_eq!(host.applications.details.size, None);
        });
        std::fs::write(&path, bytes).expect("download completed");
        host.update(cx, |host, cx| {
            host.sessions[0].build_state = BuildState::Ready;
            host.sync_application_details(Some((&app, &build)), cx);
        });
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(host.applications.details.size, Some(bytes.len() as u64));
            assert_eq!(
                host.applications.details.history.as_ref().and_then(|h| h.updated),
                Some(42)
            );
        });
    }

    #[test]
    fn history_read_returns_the_engine_rejection() {
        let directory = tempfile::tempdir().expect("isolated history fixture");
        crate::paths::private_directory(directory.path(), "control").expect("socket directory");
        let listener = std::os::unix::net::UnixListener::bind(
            directory.path().join(crate::paths::ENGINE_SOCKET),
        )
        .expect("fixture socket");
        let engine = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("history client");
            ipc::read(&mut socket).expect("history request frame");
            let rejection = Reply::Error { error: "Work Offline is enabled".into() };
            let bytes =
                ipc::codec::encode(&rejection, ipc::codec::MAX_ARCHIVE).expect("rejection frame");
            ipc::write(&mut socket, &bytes).expect("deliver rejection");
        });
        let app = AppIdentity { publisher: Id::from_bytes([3; 16]), app_id: "sketch".into() };
        let gate = worker_queue::Gate::default();
        let error = read_history_page(
            directory.path(),
            &app,
            None,
            &Mutex::new(Read::default()),
            &gate,
            gate.epoch(),
        )
        .expect_err("engine rejected history query");
        assert_eq!(error.to_string(), "Work Offline is enabled");
        engine.join().expect("engine fixture exited");
    }

    #[test]
    fn history_keeps_newest_notes_across_pages_and_the_selected_build_date() {
        let row = |id: u8, date, notes: &str| AppBuild {
            build_id: crate::identifier::encode(&[id; 32]),
            notes: notes.into(),
            published_at: date,
        };
        let selected = row(34, 42, " ");
        let build = selected.build_id.clone();
        let read = Arc::new(Mutex::new(Read::default()));
        let mut details = Details::default();
        let publisher = Id::from_bytes([1; 16]);
        let identity = AppIdentity { publisher, app_id: "sketch".into() };
        details.select(publisher, Some((&identity, &build)));
        details.active = Some(read.clone());
        let first =
            History::from_builds(&build, (0..32).map(|i| row(i, i as u64, "First page")).collect());
        details.finish(&read, Ok((first, Some(row(31, 31, "").build_id))));
        assert!(details.needs_history_page());
        assert_eq!(details.history.as_ref().expect("page received").notes.len(), MAX_NOTES);
        details.active = Some(read.clone());
        let second =
            History::from_builds(&build, vec![row(32, 41, "Latest"), row(33, 2, "Tie"), selected]);
        details.finish(&read, Ok((second, None)));
        assert!(!details.needs_history_page());
        let history = details.history.as_ref().expect("page received");
        assert_eq!(history.updated, Some(42));
        assert_eq!(history.notes.len(), MAX_NOTES);
        assert_eq!(history.notes[0].text.as_str(), "Latest");
        assert_eq!(history.notes[MAX_NOTES - 1].published_at, 2);
        assert!(history.notes.windows(2).all(|pair| {
            (std::cmp::Reverse(pair[0].published_at), &pair[0].build_id)
                <= (std::cmp::Reverse(pair[1].published_at), &pair[1].build_id)
        }));
    }

    #[test]
    fn live_reconciliation_replaces_corrected_and_cleared_notes_across_pages() {
        let build = |id: u8, published_at, notes: &str| AppBuild {
            build_id: crate::identifier::encode(&[id; 32]),
            published_at,
            notes: notes.into(),
        };
        let selected = build(2, 20, "Latest").build_id;
        let page = |builds| History::from_builds(&selected, builds);
        let read = Arc::new(Mutex::new(Read::default()));
        let mut details = Details::default();
        details.active = Some(read.clone());
        details
            .finish(&read, Ok((page(vec![build(1, 10, "First"), build(2, 20, "Latest")]), None)));
        details.invalidated = true;
        details.prepare_history_page();
        assert!(details.next.is_none(), "reconcile from the first build ID");
        details.active = Some(read.clone());
        let cursor = crate::identifier::encode(&[1; 32]);
        details
            .finish(&read, Ok((page(vec![build(1, 10, "Corrected first")]), Some(cursor.clone()))));
        assert_eq!(
            details.history.as_ref().expect("original history is visible").notes.len(),
            2,
            "retain the committed history during refresh"
        );
        // A publish between pages requires one additional complete pass, even if
        // it changes a build preceding the continuation cursor.
        details.invalidated = true;
        details.prepare_history_page();
        assert_eq!(details.next.as_deref(), Some(cursor.as_str()));
        details.active = Some(read.clone());
        details.finish(&read, Ok((page(vec![build(2, 20, "")]), None)));
        let history = details.history.as_ref().expect("replacement was committed");
        assert_eq!(history.updated, Some(20));
        assert_eq!(history.notes.len(), 1);
        assert_eq!(history.notes[0].published_at, 10);
        assert_eq!(history.notes[0].page_text().as_str(), "Corrected first");
        assert!(details.needs_history_page());
        details.prepare_history_page();
        details.active = Some(read.clone());
        details.finish(&read, Ok((page(vec![build(1, 10, ""), build(2, 20, "")]), None)));
        assert!(details.history.as_ref().expect("cleared history was committed").notes.is_empty());
        assert!(!details.needs_history_page());
    }

    #[test]
    fn failed_history_continuation_preserves_the_selected_note_page_and_retry_cursor() {
        let mut details = Details::default();
        let read = Arc::new(Mutex::new(Read::default()));
        let selected = crate::identifier::encode(&[1; 32]);
        let mut note = ReleaseNote::new(
            selected.clone(),
            format!("{}Currently reading", "a".repeat(NOTE_PAGE_BYTES)),
            42,
        );
        note.page = 1;
        let first = History { updated: Some(42), notes: vec![note] };
        details.active = Some(read.clone());
        details.finish(&read, Ok((first, Some(selected.clone()))));
        details.active = Some(read.clone());
        details.finish(&read, Err(PageError::Retry("Connection interrupted".into())));
        assert!(!details.needs_history_page());
        assert_eq!(details.history_error.as_deref(), Some("Connection interrupted"));
        assert!(details.retry_history());
        assert!(details.needs_history_page());
        assert_eq!(details.next.as_deref(), Some(selected.as_str()));
        details.active = Some(read.clone());
        details.finish(&read, Ok((History::default(), None)));
        assert!(!details.needs_history_page());
        let history = details.history.as_ref().expect("successful first page");
        assert_eq!(history.updated, Some(42));
        assert_eq!(history.notes.len(), 1);
        assert_eq!(history.notes[0].page_text().as_str(), "Currently reading");
    }

    #[test]
    fn a_visibility_rejection_discards_loaded_history_and_restarts_from_the_first_page() {
        let read = Arc::new(Mutex::new(Read::default()));
        let mut details = Details::default();
        details.active = Some(read.clone());
        details.history = Some(History { updated: Some(42), notes: Vec::new() });
        details.next = Some(crate::identifier::encode(&[1; 32]));
        let reply = Reply::Error { error: "app is unavailable".into() };
        let bytes = ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).expect("engine reply");
        let error = ipc::Response::new(bytes).err().expect("engine rejection");
        details.finish(&read, Err(PageError::from_engine(error)));
        assert!(details.history.is_none() && details.next.is_none());
        assert!(!details.needs_history_page());
        assert!(details.retry_history());
        assert!(details.needs_history_page());
    }

    #[test]
    fn note_pages_preserve_graphemes_and_split_only_oversized_clusters() {
        let prefix = "a".repeat(NOTE_PAGE_BYTES - 1);
        for grapheme in ["e\u{301}", "👨‍👩‍👧‍👦", "🇸🇪", "\r\n"] {
            let mut note = ReleaseNote::new("build".into(), format!("{prefix}{grapheme}z"), 1);
            assert_eq!(note.page_text().as_str(), prefix);
            note.page = 1;
            assert_eq!(note.page_text().as_str(), format!("{grapheme}z"));
        }
        let combining = format!("e{}", "\u{301}".repeat(NOTE_PAGE_BYTES * 2));
        let mut note = ReleaseNote::new("build".into(), combining.clone(), 1);
        let mut restored = String::new();
        for page in 0..note.page_count() {
            note.page = page;
            let chunk = note.page_text();
            assert!(!chunk.is_empty() && chunk.len() <= NOTE_PAGE_BYTES);
            restored.push_str(&chunk);
        }
        assert_eq!(restored, combining);
    }

    #[test]
    fn note_pages_bound_shaping_and_preserve_all_published_text() {
        let mut text = "é".repeat(511);
        text.push('🌱'); // The first page's byte limit bisects this character.
        text.push_str(&"\nshort line".repeat(1600));
        text.push_str(&"z".repeat(32 * 1024 - text.len() - 1600));
        assert!(crate::network::ap::operation::valid_app_notes(&text));
        let mut note = ReleaseNote::new("build".into(), text.clone(), 1);
        let mut restored = String::new();
        for page in 0..note.page_count() {
            note.page = page;
            let chunk = note.page_text();
            assert!(chunk.len() <= NOTE_PAGE_BYTES && chunk.lines().count() <= NOTE_PAGE_LINES);
            assert!(!chunk.is_empty());
            restored.push_str(&chunk);
        }
        assert_eq!(restored, text);
    }

    #[test]
    fn retry_retires_a_failed_observer_but_keeps_a_healthy_watch_during_page_retry() {
        let (socket, mut peer) = UnixStream::pair().expect("connected history observer");
        peer.set_nonblocking(true).expect("bounded peer read");
        let read = Arc::new(Mutex::new(Read { retired: false, socket: Some(socket) }));
        let mut details = Details::default();
        details.watch = Some(watch::Subscription { read: read.clone() });
        details.history_error = Some("History page read failed".into());
        assert!(details.retry_history());
        let pending = peer.read(&mut [0]).expect_err("healthy subscription stays connected");
        assert_eq!(pending.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!read.lock().expect("history observer").retired);

        details.watch_error = Some("Invalid subscription acknowledgement".into());
        assert!(details.retry_history());
        assert_eq!(peer.read(&mut [0]).expect("observer IPC shuts down"), 0);
        assert!(read.lock().expect("history observer").retired);
        assert!(details.watch_error.is_none());
        assert!(details.watch.is_some(), "hold capacity until the retired reader exits");
    }

    #[test]
    fn switching_away_and_back_interrupts_the_socket_and_discards_its_reply() {
        let account = Id::from_bytes([4; 16]);
        let app = AppIdentity { publisher: account, app_id: "first".into() };
        let other = AppIdentity { publisher: account, app_id: "second".into() };
        let build = crate::identifier::encode(&[7; 32]);
        let mut details = Details::default();
        details.select(account, Some((&app, &build)));
        let (socket, mut peer) = UnixStream::pair().expect("connected fixture");
        peer.set_nonblocking(true).expect("test read cannot block");
        let read = Arc::new(Mutex::new(Read { retired: false, socket: Some(socket) }));
        details.active = Some(read.clone());
        let (observer, mut observer_peer) = UnixStream::pair().expect("connected observer fixture");
        observer_peer.set_nonblocking(true).expect("test read cannot block");
        details.watch = Some(watch::Subscription {
            read: Arc::new(Mutex::new(Read { retired: false, socket: Some(observer) })),
        });
        details.size = Some(23);
        details.select(account, Some((&other, &build)));
        details.select(account, Some((&app, &build)));
        assert_eq!(peer.read(&mut [0]).expect("retired socket closed"), 0);
        assert_eq!(observer_peer.read(&mut [0]).expect("retired observer closed"), 0);
        assert_eq!(details.size, None);
        assert!(details.active.is_some(), "capacity belongs to the unfinished read");
        details.finish(&read, Ok((History { updated: Some(123), notes: Vec::new() }, None)));
        assert!(details.history.is_none(), "a revisited selection is a new request");
        assert!(details.active.is_none());
    }
}
