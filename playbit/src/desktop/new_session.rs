//! Device-local directory permissions take effect when the user starts a session.
mod review;
use super::*;
use crate::engine::host_mount::{HostMount, MAX_LABEL_BYTES, MAX_MOUNTS, MAX_PATH_BYTES};
use crate::ui::desktop::scrollbar::Scrollbars;
use crate::ui::desktop::zoom::length as ui;
use playbit_api::ui::CheckState;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) const FOLDER_ACCESS_TITLE: &str = "Folder access";

/// One preparation per shell, including across form replacement and worker restarts.
/// A stalled file provider cannot accumulate jobs on the shared background executor.
#[derive(Default)]
pub(super) struct Preparation(Arc<AtomicBool>);

pub(super) struct PreparationCredit(Arc<AtomicBool>);

// A stalled provider must not accumulate folder checks across closed/replaced forms.
static FOLDER_SELECTION: std::sync::LazyLock<Preparation> =
    std::sync::LazyLock::new(Preparation::default);

impl Preparation {
    pub(super) fn acquire(&self) -> Result<PreparationCredit, &'static str> {
        self.0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| PreparationCredit(self.0.clone()))
            .map_err(|_| "An app is already being prepared. Try again when it finishes.")
    }
}

impl Drop for PreparationCredit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) struct Example {
    pub app_id: &'static str,
    pub name: &'static str,
    pub seal_key: &'static str,
}

pub(super) const EXAMPLES: &[Example] = &[
    Example {
        app_id: "little-list",
        name: "Little List",
        seal_key: "PlaybitLittleListTemplateSHA256",
    },
    Example { app_id: "football", name: "Football", seal_key: "PlaybitFootballTemplateSHA256" },
];

#[derive(Clone)]
pub(super) enum Target {
    Bundle(PathBuf),
    Template { account: crate::store::Id, example: &'static Example },
    Document(String),
    Session(String),
    Folders { session: String, index: Option<usize> },
    Configuration(Box<Configuration>),
}
#[derive(Clone)]
pub(super) struct Configuration {
    instance: crate::engine::session_instance::SessionInstance,
    index: usize,
    count: usize,
}
pub(super) struct Loaded {
    configuration: Option<Configuration>,
    unavailable: Vec<String>,
}
pub(super) enum Outcome {
    Opened(guest_app::LaunchReceipt),
    Folders(Box<Loaded>),
}
pub(super) struct Mount {
    original: Option<crate::store::Id>,
    path: PathBuf,
    label: String,
    write: bool,
}
pub(super) struct Launch {
    target: Target,
    mounts: Vec<Mount>,
}
pub(super) struct Queued {
    pub launch: Launch,
    pub form: gpui::EntityId,
    pub epoch: u64,
    pub credit: worker_queue::Credit,
    pub reply: async_channel::Sender<Completion>,
}
pub(super) enum FormEvent {
    Cancel,
    Ready,
    Started,
    Reveal(std::time::Instant),
    Error(String),
    Launched(guest_app::LaunchReceipt),
}
pub(super) struct Failure {
    pub message: String,
    pub uncertain: bool,
}
pub(super) struct Completion {
    pub result: Result<Outcome, Failure>,
    pub _credit: worker_queue::Credit,
}
impl Launch {
    pub fn retained_bytes(&self) -> Option<usize> {
        if self.mounts.len() > MAX_MOUNTS {
            return None;
        }
        let target = match &self.target {
            Target::Bundle(path) if path.as_os_str().len() <= MAX_PATH_BYTES => path.capacity(),
            Target::Document(url) if url.len() <= 64 * 1024 => url.capacity(),
            Target::Session(session) if crate::identifier::valid(session, 32) => session.capacity(),
            Target::Folders { session, .. } if crate::identifier::valid(session, 32) => {
                // One registry ID list and one <=32-root configuration, plus
                // archive decoding and availability results. Never load every
                // saved instance's paths at once.
                return Some(1024 * 1024);
            }
            Target::Configuration(config) => {
                let instance = &config.instance;
                let mut bytes = std::mem::size_of::<Configuration>()
                    + instance.session.capacity()
                    + instance.build.capacity()
                    + instance.extra_volumes.capacity()
                        * std::mem::size_of::<crate::store::PersonalVolumeGrant>()
                    + std::mem::size_of_val(instance.host_mounts.as_ref());
                for mount in instance.host_mounts.iter() {
                    bytes = bytes
                        .checked_add(mount.path.capacity())?
                        .checked_add(mount.label.capacity())?;
                }
                bytes
            }
            _ => return None,
        };
        let mut bytes = target
            .checked_add(self.mounts.capacity().checked_mul(std::mem::size_of::<Mount>())?)?;
        for mount in &self.mounts {
            if mount.path.as_os_str().len() > MAX_PATH_BYTES || mount.label.len() > MAX_LABEL_BYTES
            {
                return None;
            }
            bytes =
                bytes.checked_add(mount.path.capacity())?.checked_add(mount.label.capacity())?;
        }
        // The resolved configuration and serialized request can coexist.
        bytes.checked_mul(3)?.checked_add(std::mem::size_of::<Queued>())
    }
}
fn probe(configuration: Configuration) -> Loaded {
    use crate::engine::files::{
        authority::Authority,
        host::{Directory, PrivateObjects},
    };
    let private = Arc::new(PrivateObjects::default());
    let unavailable = configuration
        .instance
        .host_mounts
        .iter()
        .map(|mount| {
            match Directory::root(
                &mount.path(),
                private.clone(),
                Arc::new(Authority::new(mount.write)),
            ) {
                Ok(_) => String::new(),
                Err(error) => format!("Unavailable: {error}"),
            }
        })
        .collect();
    Loaded { configuration: Some(configuration), unavailable }
}
fn with_bundle<T: 'static>(
    target: Target,
    access: impl Fn(&Path) -> crate::engine::Result<T> + 'static,
) -> crate::engine::Result<T> {
    match target {
        Target::Bundle(path) => access(&path),
        Target::Document(url) => file_document::with_path(&url, move |path| {
            access(path).map_err(|error| error.to_string())
        })
        .map_err(Into::into),
        _ => unreachable!("only bundle targets enter document access"),
    }
}

fn prepare_bundle(profile: &Path, bundle: &Path) -> crate::engine::Result<()> {
    match ipc::call_archive(profile, &Request::PrepareApp { bundle: bundle.to_path_buf() })?
        .into_owned()?
    {
        Reply::Developer(crate::engine::reply::DeveloperReply::AppPrepared) => Ok(()),
        Reply::Error { error } => Err(error.into()),
        _ => Err("Unexpected app preparation response".into()),
    }
}

fn prepare_template(
    profile: &Path,
    account: crate::store::Id,
    example: &Example,
) -> crate::engine::Result<(PathBuf, review::AppReview)> {
    use crate::engine::reply::DeveloperReply;
    use crate::package::Package;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    const MAX_TEMPLATE_BYTES: usize = 1024 * 1024;
    let channel = crate::engine::profile::for_profile(profile)?;
    let (suffix, seal_suffix) = match channel {
        crate::release_channel::ReleaseChannel::Public => ("", ""),
        crate::release_channel::ReleaseChannel::Testing => ("-testing", "Testing"),
        _ => ("-personal", "Personal"),
    };
    let filename = format!("{}{suffix}.pbapp", example.app_id);
    let bundle = crate::ui::desktop::bundled_example_dir(example.app_id)?.join(&filename);
    let mut bytes = Vec::new();
    file_document::open_regular(&bundle)?
        .take((MAX_TEMPLATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_TEMPLATE_BYTES {
        return Err(format!("Bundled {} template exceeds 1 MiB", example.name).into());
    }
    let package = Package::from_bytes(&bytes)?;
    if package.app_id() != example.app_id {
        return Err(format!("The bundled {} template is invalid.", example.name).into());
    }
    super::guest_runtime::verify_template(example, seal_suffix, &bytes)?;
    if !channel.is_personal() && package.publisher_certificate().is_none() {
        return Err("The bundled example has no publisher certificate.".into());
    }

    let staging = crate::paths::private_directory(profile, crate::paths::STAGING)?;
    let temporary = staging.join(format!("template-{}", crate::store::Id::generate()?));
    fs::DirBuilder::new().mode(0o700).create(&temporary)?;
    let result = (|| -> crate::engine::Result<_> {
        let bundle = temporary.join(filename);
        let mut file =
            fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&bundle)?;
        file.write_all(&bytes)?;
        drop(file);
        drop(bytes);
        let signed = if package.publisher_certificate().is_some() {
            package
        } else {
            // Previously shipped unsigned templates become a private source build.
            let reply =
                ipc::call_archive(profile, &Request::SignPackage { bundle: bundle.clone() })?
                    .into_owned()?;
            let build_id = match reply {
                Reply::Developer(DeveloperReply::PackageSigned { build_id }) => build_id,
                Reply::Error { error } => return Err(error.into()),
                _ => return Err("Unexpected template signing response".into()),
            };
            let signed = Package::open(&bundle)?;
            if signed.id()? != build_id
                || signed
                    .publisher_certificate()
                    .is_none_or(|cert| cert.claims.publisher != account)
            {
                return Err("The signed template differs from the requested app or account.".into());
            }
            signed
        };
        prepare_bundle(profile, &bundle)?;
        let review = review::AppReview::from_package(&signed)?;
        Ok((profile.join(crate::paths::BUILDS).join(&review.build), review))
    })();
    let cleanup = fs::remove_dir_all(temporary);
    result.and_then(|bundle| {
        cleanup?;
        Ok(bundle)
    })
}

pub(super) fn execute(
    profile: &Path,
    launch: Launch,
    focus: bool,
    created: impl FnOnce(SessionRow),
    completion: Option<guest_app::LaunchCompletion>,
) -> Result<Outcome, Failure> {
    if let Target::Folders { session, index } = &launch.target {
        let result = (|| -> crate::engine::Result<Outcome> {
            let Reply::InstanceConfigurations(ids) = ipc::call_archive(
                profile,
                &Request::InstanceConfigurations { session: session.clone() },
            )?
            .into_owned()?
            else {
                return Err("Could not load saved folder configurations".into());
            };
            if ids.is_empty() {
                return Ok(Outcome::Folders(Box::new(Loaded {
                    configuration: None,
                    unavailable: Vec::new(),
                })));
            }
            let index = index.unwrap_or(ids.len() - 1).min(ids.len() - 1);
            let Reply::Instance(instance) = ipc::call_archive(
                profile,
                &Request::InstanceConfiguration { instance: ids[index] },
            )?
            .into_owned()?
            else {
                return Err("Could not load this instance’s folders".into());
            };
            Ok(Outcome::Folders(Box::new(probe(Configuration {
                instance,
                index,
                count: ids.len(),
            }))))
        })();
        return result.map_err(|error| Failure { message: error.to_string(), uncertain: false });
    }
    let mut uncertain = false;
    let result = (|| -> crate::engine::Result<Outcome> {
        let mounts = launch
            .mounts
            .iter()
            .map(|mount| {
                if let Target::Configuration(config) = &launch.target
                    && let Some(original) = config
                        .instance
                        .host_mounts
                        .iter()
                        .find(|original| Some(original.id) == mount.original)
                    && original.path() == mount.path
                    && original.write == mount.write
                {
                    let mut original = original.clone();
                    original.label = mount.label.clone();
                    return Ok(original);
                }
                HostMount::resolve(&mount.path, Some(&mount.label), mount.write)
            })
            .collect::<crate::engine::Result<Vec<_>>>()?;
        crate::engine::host_mount::validate(&mounts)?;
        if let Target::Configuration(mut config) = launch.target {
            uncertain = true;
            let reply = ipc::call_archive(
                profile,
                &Request::InstanceMounts {
                    instance: config.instance.id,
                    expected: config.instance.host_mounts.clone(),
                    mounts: mounts.into_boxed_slice(),
                },
            )?
            .into_owned()?;
            match reply {
                Reply::Instance(instance) => {
                    config.instance = instance;
                    return Ok(Outcome::Folders(Box::new(probe(*config))));
                }
                Reply::Error { error } => return Err(error.into()),
                _ => {
                    return Err(
                        "Folder changes could not be confirmed; reload to check them".into()
                    );
                }
            }
        }
        let session = match launch.target {
            target @ (Target::Bundle(_) | Target::Document(_)) => {
                // Coordination can fail before or after its synchronous accessor.
                // Only an attempted mutation makes the outcome uncertain.
                let attempted = std::rc::Rc::new(std::cell::Cell::new(false));
                let observed = attempted.clone();
                let profile = profile.to_path_buf();
                let response = with_bundle(target, move |bundle| {
                    let app_name = crate::package::Package::read_display_name(
                        &mut file_document::open_regular(bundle)?,
                    )?;
                    observed.set(true);
                    let response = ipc::call_archive(
                        &profile,
                        &Request::CreateSession {
                            bundle: bundle.to_path_buf(),
                            root: None,
                            user: None,
                        },
                    )?
                    .into_owned()?;
                    Ok((app_name, response))
                });
                uncertain = attempted.get();
                let (app_name, response) = response?;
                match response {
                    Reply::Developer(crate::engine::reply::DeveloperReply::Session(session)) => {
                        created(SessionRow {
                            reference: String::new(),
                            id: session.id.clone(),
                            resolved_build_id: session.resolved_build_id,
                            app_name: Some(app_name),
                            build_state: crate::engine::builds::BuildState::Ready,
                        });
                        session.id
                    }
                    Reply::Error { error } => return Err(error.into()),
                    _ => return Err("Unexpected new-session reply".into()),
                }
            }
            Target::Session(session) => session,
            Target::Folders { .. } | Target::Configuration(_) | Target::Template { .. } => {
                unreachable!("only prepared app targets enter launch")
            }
        };
        let prepared =
            ipc::call_archive(profile, &Request::PrepareSession { session })?.into_owned()?;
        match prepared {
            Reply::SessionPrepared(prepared) => {
                uncertain = true;
                let instance = match completion {
                    Some(completion) => guest_app::launch_async(
                        profile,
                        &prepared.session,
                        true,
                        focus,
                        mounts,
                        true,
                        completion,
                    )?,
                    None => guest_app::launch_confirmed(
                        profile,
                        &prepared.session,
                        true,
                        focus,
                        mounts,
                        true,
                    )?,
                };
                Ok(Outcome::Opened(instance))
            }
            Reply::Error { error } => Err(error.into()),
            _ => Err("The session is not ready to open".into()),
        }
    })();
    result.map_err(|error| Failure { message: error.to_string(), uncertain })
}
// Picker-only validation runs off the UI thread; loading saved configurations preserves all rows.
fn selected_folder(
    path: PathBuf,
    existing: &[(u32, PathBuf)],
    replace: Option<u32>,
) -> Result<PathBuf, String> {
    if path.as_os_str().len() > MAX_PATH_BYTES {
        return Err("The selected path is too long.".into());
    }
    let resolved =
        path.canonicalize().map_err(|error| format!("Could not open folder: {error}"))?;
    if !resolved.is_dir() {
        return Err("Choose a folder.".into());
    }
    for (id, prior) in existing {
        if Some(*id) != replace
            && (prior == &path || prior.canonicalize().is_ok_and(|prior| prior == resolved))
        {
            return Err("This folder is already listed. Use its existing access controls.".into());
        }
    }
    Ok(path)
}

struct Row {
    id: u32,
    original: Option<crate::store::Id>,
    unavailable: String,
    path: PathBuf,
    label: Entity<text_editor::TextEditor>,
    write: bool,
}
pub(super) struct Form {
    target: Target,
    template_account: Option<crate::store::Id>,
    review: Option<review::AppReview>,
    join: Option<review::Join>,
    owner: Option<gpui::WeakEntity<Shell>>,
    send: RequestSender,
    rows: Vec<Row>,
    next: u32,
    picker: bool,
    focus_add_folder: bool,
    busy: bool,
    prepared: bool,
    preparing: Option<review::PreparationState>,
    opened: bool,
    launched: Option<guest_app::LaunchReceipt>,
    progress: Option<super::launch_progress::Progress>,
    uncertain: bool,
    error: String,
    controls: BTreeMap<(u32, u8), Entity<controls::Control>>,
    texts: BTreeMap<(u32, u8), Entity<()>>,
    scrollbars: Option<Entity<Scrollbars>>,
}
impl gpui::EventEmitter<FormEvent> for Form {}

impl Shell {
    pub(super) fn show_app_document(&mut self, url: String, cx: &mut Context<Self>) {
        self.cancel_invitation_restoration();
        self.show_new_session(Target::Document(url), cx);
        self.initial_link = false;
        self.show_initial_feedback(cx);
    }

    pub(super) fn show_new_session(&mut self, target: Target, cx: &mut Context<Self>) {
        let preparation = target.clone();
        let form = cx.new(|_| Form::new(target, self.send.clone()));
        form.update(cx, |form, cx| form.prepare_target(preparation, &self.app_preparation, cx));
        self.show_session_form(form, cx);
    }

    pub(super) fn show_session_form(&mut self, form: Entity<Form>, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        form.update(cx, |form, _| {
            if let Target::Session(session) = &form.target
                && form.review.is_none()
                && let Some(row) = self.sessions.iter().find(|row| &row.id == session)
            {
                let app = self
                    .applications
                    .sessions()
                    .iter()
                    .find(|entry| &entry.id == session)
                    .map(|entry| entry.app.clone());
                form.review = Some(review::AppReview {
                    name: row.app_name.clone().unwrap_or_else(|| "App unavailable".into()),
                    build: row.resolved_build_id.clone(),
                    app,
                    icon: self.session_icons.get(&row.resolved_build_id),
                });
            }
            form.owner = Some(owner);
        });
        let send = self.send.clone();
        cx.subscribe(&form, move |host, form, event, cx| {
            if host.signing_out || !host.send.same_worker(&send) {
                return;
            }
            match event {
                FormEvent::Cancel => {
                    if host.new_session.as_ref() == Some(&form) && form.read(cx).can_cancel() {
                        host.dismiss_launch_dialog(cx);
                    }
                }
                FormEvent::Ready => {
                    if host.new_session.as_ref() == Some(&form)
                        && let Target::Session(session) = &form.read(cx).target
                    {
                        host.select_sidebar_session(session.clone(), cx);
                    }
                }
                FormEvent::Started => {
                    if matches!(
                        form.read(cx).target,
                        Target::Folders { .. } | Target::Configuration(_)
                    ) {
                        return;
                    }
                    // Form admission checks both retained operations and credits.
                    assert!(host.launch_forms.len() < 32, "bounded admitted forms");
                    host.launch_forms.insert(form.entity_id(), form.clone());
                    let session = match &form.read(cx).target {
                        Target::Session(session) => Some(session.clone()),
                        _ => None,
                    };
                    if let Some(session) = session {
                        let mut progress = form.read(cx).progress.clone().expect("admitted launch");
                        progress.form = Some(form.entity_id());
                        let started = progress.started;
                        host.launches.insert(session.clone(), progress);
                        host.launch_deadlines(session, started, false, cx);
                    }
                }
                FormEvent::Reveal(started) => {
                    for progress in host.launches.values_mut().filter(|progress| {
                        progress.form == Some(form.entity_id()) && progress.started == *started
                    }) {
                        progress.visible = true;
                    }
                }
                FormEvent::Error(error) => {
                    host.launch_forms.remove(&form.entity_id());
                    host.launches.retain(|_, progress| progress.form != Some(form.entity_id()));
                    if host.new_session.as_ref() != Some(&form) {
                        host.error.clone_from(error);
                    }
                }
                FormEvent::Launched(instance) => {
                    for progress in host
                        .launches
                        .values_mut()
                        .filter(|progress| progress.form == Some(form.entity_id()))
                    {
                        progress.instance = Some(*instance);
                    }
                    host.reconcile_launches(cx);
                }
            }
            cx.notify();
        })
        .detach();
        if form.read(cx).reviewed_invitation().is_none() {
            self.leave_invitation();
        }
        self.new_session = Some(form);
        cx.notify();
    }

    pub(super) fn session_created(
        &mut self,
        form: gpui::EntityId,
        row: SessionRow,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.iter().any(|session| session.id == row.id) {
            if self.sessions.len() == 256 {
                self.error = "Session list exceeds its 256-session display limit".into();
                if let Some(form) = self.launch_forms.remove(&form) {
                    form.update(cx, |form, cx| form.finish_launch(Some(self.error.clone()), cx));
                }
                cx.notify();
                return;
            }
            self.sessions.push(row.clone());
        }
        // Creation and the terminal receipt have independent delivery paths.
        // Keep the admitted form until transfer, including after navigation.
        let admitted = self.launch_forms.remove(&form).or_else(|| {
            self.new_session.as_ref().filter(|current| current.entity_id() == form).cloned()
        });
        let mut error = String::new();
        if let Some(admitted) = admitted {
            let state = admitted.read(cx);
            error.clone_from(&state.error);
            if error.is_empty()
                && let Some(mut progress) = state.progress.clone()
            {
                progress.form = Some(form);
                progress.instance = state.launched;
                progress.phase =
                    crate::engine::session_instance::StartupPhase::PreparingApplication;
                let started = progress.started;
                self.launches.insert(row.id.clone(), progress);
                self.launch_deadlines(row.id.clone(), started, false, cx);
                self.reconcile_launches(cx);
            }
        }
        // A delayed creation must not undo navigation away from its form.
        if self.new_session.as_ref().is_some_and(|current| current.entity_id() == form) {
            self.select_sidebar_session(row.id, cx);
            if !error.is_empty() {
                self.error = error;
            }
        }
        cx.notify();
    }
}

impl Form {
    pub(super) fn set_launch_phase(
        &mut self,
        phase: crate::engine::session_instance::StartupPhase,
        cx: &mut Context<Self>,
    ) {
        if let Some(progress) = &mut self.progress {
            progress.phase = phase;
        }
        cx.notify();
    }

    pub(super) fn finish_launch(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.busy = false;
        self.progress = None;
        self.opened = error.is_none();
        self.uncertain = error.is_some();
        self.error = error.unwrap_or_default();
        if self.opened {
            cx.emit(FormEvent::Ready);
        }
        cx.notify();
    }

    fn prepare_target(&mut self, target: Target, admission: &Preparation, cx: &mut Context<Self>) {
        let credit = match admission.acquire() {
            Ok(credit) => credit,
            Err(error) => {
                self.error = error.into();
                cx.notify();
                return;
            }
        };
        self.preparing = Some(review::PreparationState::Reading);
        let profile = profile_path();
        let work = cx.background_executor().spawn(async move {
            let result = match target {
                Target::Template { account, example } => {
                    prepare_template(&profile, account, example)
                }
                target => with_bundle(target, move |bundle| {
                    let review =
                        review::AppReview::from_package(&crate::package::Package::open(bundle)?)?;
                    prepare_bundle(&profile, bundle)?;
                    // Launch from the immutable build reviewed here, even if the source file moves.
                    let cached = profile.join(crate::paths::BUILDS).join(&review.build);
                    if !cached.is_file() {
                        return Err("The app changed during preparation. Open it again.".into());
                    }
                    Ok((cached, review))
                }),
            }
            .map_err(|error| error.to_string());
            // Keep admission through foreground completion, even for a dropped form.
            (result, credit)
        });
        cx.spawn(async move |form, cx| {
            let (result, _credit) = work.await;
            let _ = form.update(cx, |form, cx| {
                let start = form.preparing.take() == Some(review::PreparationState::StartWhenReady);
                match result {
                    Ok((bundle, review)) => {
                        form.target = Target::Bundle(bundle);
                        form.review = Some(review);
                        form.prepared = true;
                        if start
                            && form.owner.as_ref().and_then(gpui::WeakEntity::upgrade).is_some_and(
                                |owner| {
                                    let host = owner.read(cx);
                                    !host.signing_out
                                        && host.send.same_worker(&form.send)
                                        && host.new_session.as_ref().is_some_and(|current| {
                                            current.entity_id() == cx.entity_id()
                                        })
                                },
                            )
                        {
                            form.open(cx);
                        }
                        if let Some(progress) = &mut form.progress {
                            progress.phase =
                                crate::engine::session_instance::StartupPhase::CreatingSession;
                        }
                    }
                    Err(error) if !form.busy && !form.opened => form.error = error,
                    Err(_) => {} // The admitted launch reports its own preparation failure.
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn creates_session(&self) -> bool {
        matches!(self.target, Target::Bundle(_) | Target::Template { .. } | Target::Document(_))
    }
    pub(super) fn scrollbars(&mut self, cx: &mut Context<Self>) -> Entity<Scrollbars> {
        self.scrollbars.get_or_insert_with(|| cx.new(|_| Scrollbars::new(false))).clone()
    }
    pub fn new(target: Target, send: RequestSender) -> Self {
        let template_account = match &target {
            Target::Template { account, .. } => Some(*account),
            _ => None,
        };
        Self {
            target,
            template_account,
            review: None,
            join: None,
            owner: None,
            send,
            rows: Vec::new(),
            next: 1,
            picker: false,
            focus_add_folder: false,
            busy: false,
            prepared: false,
            preparing: None,
            opened: false,
            launched: None,
            progress: None,
            uncertain: false,
            error: String::new(),
            controls: BTreeMap::new(),
            texts: BTreeMap::new(),
            scrollbars: None,
        }
    }
    pub fn folders(session: String, send: RequestSender, cx: &mut Context<Self>) -> Self {
        let mut form = Self::new(Target::Folders { session, index: None }, send);
        form.open(cx);
        form
    }
    fn load(
        &mut self,
        configuration: Configuration,
        unavailable: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.rows.clear();
        self.controls.retain(|key, _| key.0 == 0);
        self.texts.retain(|key, _| key.0 == 0);
        for (mount, unavailable) in configuration.instance.host_mounts.iter().zip(unavailable) {
            self.add(mount.path(), cx);
            if let Some(row) = self.rows.last_mut() {
                row.original = Some(mount.id);
                row.unavailable = unavailable;
                row.write = mount.write;
                row.label = cx.new(|cx| {
                    text_editor::TextEditor::new(&mount.label, cx)
                        .placeholder("Folder label")
                        .bound_edits(MAX_LABEL_BYTES)
                });
            }
        }
        self.target = Target::Configuration(Box::new(configuration));
        self.uncertain = false;
        self.opened = false;
    }
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.picker {
            return;
        }
        self.uncertain = false;
        self.opened = false;
        if let Target::Configuration(config) = &self.target {
            self.page(config.index, cx);
        } else if matches!(self.target, Target::Folders { .. }) {
            self.open(cx);
        }
    }
    fn page(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy || self.picker {
            return;
        }
        let Target::Configuration(config) = &self.target else { return };
        self.target =
            Target::Folders { session: config.instance.session.clone(), index: Some(index) };
        self.rows.clear();
        self.uncertain = false;
        self.opened = false;
        self.open(cx);
    }
    fn button(
        &mut self,
        key: (u32, u8),
        label: &str,
        disabled: bool,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        self.control(
            key,
            controls::Kind::Button(playbit_api::ui::ButtonStyle::Default),
            label,
            disabled,
            move |_, this, cx| action(this, cx),
            cx,
        )
    }
    fn control(
        &mut self,
        key: (u32, u8),
        kind: controls::Kind,
        label: &str,
        disabled: bool,
        action: impl Fn(u32, &mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let weak = cx.weak_entity();
        let activate: controls::Activate = Rc::new(move |id, cx| {
            let _ = weak.update(cx, |this, cx| {
                action(id, this, cx);
                cx.notify();
            });
        });
        let theme = theme();
        let properties = controls::Properties { kind, label, disabled, theme: &theme };
        let control = self.controls.entry(key).or_insert_with(|| {
            cx.new(|cx| controls::Control::new(0, properties, activate.clone(), cx))
        });
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
        });
        control.clone()
    }
    fn remove_folder(&mut self, id: u32) {
        self.rows.retain(|row| row.id != id);
        self.controls.retain(|key, _| key.0 != id);
        self.texts.retain(|key, _| key.0 != id);
    }

    fn access(
        &mut self,
        id: u32,
        write: bool,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let theme = theme();
        let properties = controls::Properties {
            kind: controls::Kind::Checkbox(if write { CheckState::On } else { CheckState::Off }),
            label: "Allow changes",
            disabled,
            theme: &theme,
        };
        let weak = cx.weak_entity();
        let control = self.controls.entry((id, 1)).or_insert_with(|| {
            cx.new(|cx| {
                controls::Control::new(
                    0,
                    properties,
                    Rc::new(move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            if !this.busy
                                && !this.picker
                                && !this.opened
                                && !this.uncertain
                                && let Some(row) = this.rows.iter_mut().find(|row| row.id == id)
                            {
                                row.write = !row.write;
                                cx.notify();
                            }
                        });
                    }),
                    cx,
                )
            })
        });
        control.update(cx, |control, cx| control.set(properties, cx));
        control.clone()
    }
    fn text(
        &mut self,
        key: (u32, u8),
        text: String,
        heading: bool,
        live: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let identity = self.texts.entry(key).or_insert_with(|| cx.new(|_| ()));
        host_text::element(
            text.into(),
            text_editor::accessibility_text_ids(identity.entity_id()).0,
            heading,
            live,
        )
    }
    fn pick(&mut self, replace: Option<u32>, cx: &mut Context<Self>) {
        if self.picker
            || self.busy
            || self.opened
            || self.uncertain
            || (replace.is_none() && self.rows.len() == MAX_MOUNTS)
        {
            return;
        }
        let credit = match FOLDER_SELECTION.acquire() {
            Ok(credit) => credit,
            Err(_) => {
                self.error =
                    "A folder is already being checked. Try again when it finishes.".into();
                return;
            }
        };
        self.picker = true;
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Allow folder access".into()),
        });
        let existing = self.rows.iter().map(|row| (row.id, row.path.clone())).collect::<Vec<_>>();
        cx.spawn(async move |this, cx| {
            let _credit = credit;
            let result = match paths.await {
                Ok(Ok(Some(mut paths))) if paths.len() == 1 => {
                    let path = paths.remove(0);
                    cx.background_executor()
                        .spawn(async move { selected_folder(path, &existing, replace).map(Some) })
                        .await
                }
                Ok(Ok(None)) => Ok(None),
                _ => Err("Could not select a folder.".into()),
            };
            let _ = this.update(cx, |this, cx| {
                this.picker = false;
                match result {
                    Ok(Some(path)) => {
                        if let Some(id) = replace {
                            if let Some(row) = this.rows.iter_mut().find(|row| row.id == id) {
                                row.path = path;
                                row.original = None;
                                row.unavailable.clear();
                                this.error.clear();
                            }
                        } else {
                            this.add(path, cx);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => this.error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn add(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.rows.len() >= MAX_MOUNTS || path.as_os_str().len() > MAX_PATH_BYTES {
            self.error = "The folder selection exceeds this session’s limits.".into();
            return;
        }
        let Some(next) = self.next.checked_add(1) else {
            self.error = "Close this form and start again.".into();
            return;
        };
        let label = path.file_name().and_then(|name| name.to_str()).unwrap_or("host");
        let label = if label.len() <= MAX_LABEL_BYTES { label } else { "" };
        let launch = self.is_launch_dialog();
        let editor = cx.new(|cx| {
            let editor = text_editor::TextEditor::new(label, cx)
                .placeholder("Folder label")
                .bound_edits(MAX_LABEL_BYTES);
            if launch { editor.inspector_field() } else { editor }
        });
        self.rows.push(Row {
            id: self.next,
            original: None,
            unavailable: String::new(),
            path,
            label: editor,
            write: false,
        });
        self.next = next;
        self.error.clear();
    }
    fn open(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || self.picker
            || self.opened
            || self.uncertain
            || matches!(self.target, Target::Template { .. })
        {
            return;
        }
        if self.is_launch_dialog()
            && let Some(owner) = self.owner.as_ref().and_then(gpui::WeakEntity::upgrade)
        {
            let host = owner.read(cx);
            if self.template_account.is_some_and(|account| account != host.account.id) {
                self.error = "The account changed. Go back and open the example again.".into();
                return;
            }
            if host.launch_count() >= 32 {
                self.error = "Too many sessions are opening.".into();
                return;
            }
            if let Target::Session(session) = &self.target
                && host.launches.contains_key(session)
            {
                self.error = "This session is already opening.".into();
                return;
            }
        }
        let launch = Launch {
            target: self.target.clone(),
            mounts: self
                .rows
                .iter()
                .map(|row| Mount {
                    original: row.original,
                    path: row.path.clone(),
                    label: row.label.read(cx).text().to_owned(),
                    write: row.write,
                })
                .collect(),
        };
        let mut progress = super::launch_progress::Progress::new();
        crate::trace::mark("guest.launch.click");
        let receive = match self.send.launch(launch, cx.entity_id()) {
            Ok(receive) => receive,
            Err(error) => {
                self.error = error.into();
                return;
            }
        };
        self.busy = true;
        if self.prepared {
            progress.phase = crate::engine::session_instance::StartupPhase::CreatingSession;
        }
        self.progress = Some(progress);
        cx.emit(FormEvent::Started);
        self.error.clear();
        cx.notify();
        let started = self.progress.as_ref().expect("new progress").started;
        // One reveal deadline follows this operation through form-to-row transfer.
        let indicator = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200).saturating_sub(started.elapsed()))
                .await;
            let _ = indicator.update(cx, |form, cx| {
                if let Some(progress) =
                    form.progress.as_mut().filter(|progress| progress.started == started)
                {
                    progress.visible = true;
                    cx.emit(FormEvent::Reveal(started));
                    cx.notify();
                }
            });
        })
        .detach();
        // Keep the launch's error owner alive after creation closes the form.
        let form = cx.entity();
        cx.spawn(async move |_, cx| {
            let completion = receive.recv().await;
            let _ = form.update(cx, |this, cx| {
                match completion {
                    Ok(Completion { result: Ok(Outcome::Opened(instance)), .. }) => {
                        this.launched = Some(instance);
                        cx.emit(FormEvent::Launched(instance));
                    },
                    Ok(Completion { result: Ok(Outcome::Folders(loaded)), .. }) => {
                        this.busy = false;
                        if let Some(config) = loaded.configuration {
                            this.load(config, loaded.unavailable, cx);
                        } else {
                            this.error = "No saved GUI instances. Open a session to choose its folders.".into();
                        }
                    },
                    Ok(Completion { result: Err(error), .. }) => {
                        this.busy = false;
                        this.uncertain = error.uncertain;
                        this.error = error.message;
                        if this.uncertain {
                            this.error.push_str(if matches!(this.target, Target::Configuration(_)) { " Reload folders to check the saved state before editing again." } else { " Check Sessions before opening again." });
                        }
                    },
                    Err(_) => {
                        this.busy = false;
                        this.uncertain = true;
                        this.error = "The action could not be confirmed. Reload folders or check Sessions before trying again.".into();
                    },
                }
                if !this.error.is_empty() {
                    cx.emit(FormEvent::Error(this.error.clone()));
                }
                cx.notify();
            });
        }).detach();
    }
}
impl Render for Form {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.is_launch_dialog() {
            return self.launch_review(window, cx);
        }
        let mut body = div().flex().flex_col().flex_none().w_full().min_w_0().gap_3()
            .child(self.text((0, 0), FOLDER_ACCESS_TITLE.into(), true, false, cx).text_2xl())
            .child(self.text((0, 1), "Choose folders this session window can read on this Mac. Each window has its own folder access. Allow changes to let the app edit files. The app may share folder contents with other people in the session.".into(), false, false, cx));
        let disabled = self.busy
            || self.picker
            || self.uncertain
            || matches!(self.target, Target::Folders { .. });
        if matches!(self.target, Target::Folders { .. }) {
            let reload = self.button(
                (0, 5),
                "Reload folders",
                self.busy || self.picker,
                |this, cx| this.reload(cx),
                cx,
            );
            body = body.child(reload);
        }
        if let Target::Configuration(config) = &self.target {
            let index = config.index;
            let count = config.count;
            let label = format!("Instance {} of {} · {}", index + 1, count, config.instance.id);
            let previous = self.button(
                (0, 3),
                "Previous instance",
                self.busy || self.picker || index == 0,
                move |this, cx| this.page(index - 1, cx),
                cx,
            );
            let next = self.button(
                (0, 4),
                "Next instance",
                self.busy || self.picker || index + 1 == count,
                move |this, cx| this.page(index + 1, cx),
                cx,
            );
            let reload = self.button(
                (0, 5),
                "Reload folders",
                self.busy || self.picker,
                move |this, cx| this.page(index, cx),
                cx,
            );
            body = body
                .child(self.text((0, 3), label, false, false, cx).text_size(ui(12.)))
                .child(div().flex().gap_2().child(previous).child(next).child(reload));
        }
        let rows = self
            .rows
            .iter()
            .map(|row| {
                (row.id, row.path.clone(), row.label.clone(), row.write, row.unavailable.clone())
            })
            .collect::<Vec<_>>();
        for (id, path, label, write, unavailable) in rows {
            label.update(cx, |editor, cx| editor.set_read_only(disabled, &theme(), cx));
            let access = self.access(id, write, disabled, cx);
            let path_label = path.display().to_string();
            let reveal = self.button(
                (id, 4),
                "Show in Finder",
                false,
                move |_, cx| cx.reveal_path(&path),
                cx,
            );
            let path_text = self.text((id, 0), path_label, false, false, cx).text_size(ui(12.));
            let remove = self.button(
                (id, 2),
                "Remove",
                disabled,
                move |this, _| {
                    if !this.busy && !this.picker && !this.uncertain {
                        this.remove_folder(id);
                    }
                },
                cx,
            );
            let repair = self.button(
                (id, 3),
                if unavailable.is_empty() { "Choose folder…" } else { "Repair…" },
                disabled,
                move |this, cx| this.pick(Some(id), cx),
                cx,
            );
            let status_empty = unavailable.is_empty();
            let status = self
                .text((id, 1), unavailable, false, false, cx)
                .text_color(rgb(0xa32820))
                .text_size(ui(12.));
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .min_w_0()
                    .gap_2()
                    .p_3()
                    .bg(rgb(0xffffff))
                    .rounded_md()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w_0().text_ellipsis().child(path_text))
                            .child(reveal),
                    )
                    .when(!status_empty, |row| row.child(status))
                    .child(div().w_full().min_w_0().child(label))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(access)
                            .child(repair)
                            .child(remove),
                    ),
            );
        }
        let add = self.button(
            (0, 1),
            "Add folder…",
            disabled || self.rows.len() == MAX_MOUNTS,
            |this, cx| this.pick(None, cx),
            cx,
        );
        let open = self.button((0, 2), "Save folders", disabled, |this, cx| this.open(cx), cx);
        let mut actions = div().flex().flex_wrap().items_center().gap_2().child(add).child(open);
        if self.busy {
            window.request_animation_frame();
            let angle = self.progress.as_ref().map_or(0., |progress| {
                progress.started.elapsed().as_secs_f32() * std::f32::consts::TAU
            });
            actions = actions.child(
                gpui::svg()
                    .path("progress-spinner")
                    .size(ui(16.))
                    .text_color(rgb(0x000000))
                    .with_transformation(gpui::Transformation::rotate(gpui::Radians(angle))),
            );
            if let Some(progress) = self.progress.as_ref().filter(|progress| progress.visible) {
                actions = actions.child(
                    self.text((0, 4), progress.phase.label().into(), false, true, cx)
                        .text_size(ui(12.)),
                );
            }
        }
        body = body.child(actions);
        if !self.error.is_empty() {
            body = body.child(
                self.text((0, 2), self.error.clone(), false, true, cx).text_color(rgb(0xa32820)),
            );
        }
        body
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests exercise admitted launch drafts and native controls.")]
mod tests {
    #[gpui::test]
    fn finder_file_delivery_uses_the_new_session_flow(cx: &mut gpui::TestAppContext) {
        // The same callback handles file delivery before or after the initial window.
        for initial_link in [true, false] {
            let (host, _window, requests) = super::test_support::host(cx);
            host.update(cx, |host, cx| {
                host.initial_link = initial_link;
                host.open_url_result(Ok("file:///tmp/My%20caf%C3%A9.pbapp".into()), cx);
                let form = host.new_session.as_ref().unwrap().read(cx);
                assert!(
                    matches!(&form.target, super::Target::Document(url) if url.ends_with(".pbapp"))
                );
                assert!(form.is_launch_dialog());
                assert!(host.pending_database.is_none());
                assert!(!host.initial_link, "file opens must reveal the session form");
            });
            assert!(requests.try_recv().is_err(), "file delivery alone does not create a session");
        }
    }
    use super::*;

    #[test]
    fn example_review_keeps_its_signed_build_and_publisher() {
        let original = crate::package::Package::new(
            "little-list".into(),
            "Little List".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::from([("assets/sample.txt".into(), b"sample".to_vec())]),
        )
        .unwrap();
        let signed = crate::package::signature::testing::authenticate(
            &original,
            crate::store::Id::from_bytes([7; 16]),
            10,
        )
        .unwrap();
        let review = review::AppReview::from_package(&signed).unwrap();
        assert_eq!(review.name, "Little List");
        assert_eq!(review.build, signed.id().unwrap());
        assert_eq!(review.app.unwrap().publisher, crate::store::Id::from_bytes([7; 16]));
    }

    #[gpui::test]
    fn original_invitation_names_only_its_certified_example_publisher(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::release_channel::ReleaseChannel;
        use crate::store::{BuildSelector, Id, ReceivedInvitation, Session, UpdateAuthority};
        let (host, _, _) = test_support::host(cx);
        let build = "HfGyXEJgBcfzT6d4S4wS35ZNCaiUypjP4vTfYWaQKsN6";
        let publisher = Id::parse("R8UTnVVXKZeS5FLfoLmwVt").unwrap();
        let mut invitation = ReceivedInvitation {
            session: Session {
                id: crate::identifier::encode(&[3; 32]),
                selector: BuildSelector::Exact { build_id: build.into() },
                resolved_build_id: build.into(),
                app: crate::package::AppIdentity { app_id: "football".into(), publisher },
                root_volume: Id::from_bytes([1; 16]),
                shared_volume: None,
                creator: Id::from_bytes([2; 16]),
                update_authority: UpdateAuthority::Creator,
            },
            root_name: "Playbit".into(),
        };
        let label = |invitation: &ReceivedInvitation, realm| {
            host.read_with(cx, |host, _| {
                Form::invitation(invitation, host.send.clone())
                    .review
                    .unwrap()
                    .published_name(realm)
            })
        };
        assert_eq!(label(&invitation, ReleaseChannel::Testing), Some("Playbit"));
        assert_eq!(label(&invitation, ReleaseChannel::Public), None);
        invitation.session.app.app_id = "pretender".into();
        assert_eq!(label(&invitation, ReleaseChannel::Testing), None);
        invitation.session.app.app_id = "football".into();
        invitation.session.app.publisher = Id::from_bytes([4; 16]);
        assert_eq!(label(&invitation, ReleaseChannel::Testing), None);
        invitation.session.app.publisher = publisher;
        invitation.session.resolved_build_id = crate::identifier::encode(&[5; 32]);
        assert_eq!(label(&invitation, ReleaseChannel::Testing), None);
    }

    #[gpui::test]
    fn template_launch_waits_for_its_prepared_bundle_and_account(cx: &mut gpui::TestAppContext) {
        let (host, _, requests) = test_support::host(cx);
        let form = host.update(cx, |host, cx| {
            let form = cx.new(|_| {
                Form::new(
                    Target::Template { account: host.account.id, example: &EXAMPLES[0] },
                    host.send.clone(),
                )
            });
            host.show_session_form(form.clone(), cx);
            form
        });
        form.update(cx, |form, cx| form.open(cx));
        assert!(requests.try_recv().is_err());

        let prepared = PathBuf::from("/tmp/prepared-template.pbapp");
        let account = host.update(cx, |host, _| {
            std::mem::replace(&mut host.account.id, crate::store::Id::from_bytes([7; 16]))
        });
        form.update(cx, |form, cx| {
            form.target = Target::Bundle(prepared.clone());
            form.open(cx);
            assert!(form.error.contains("account changed"));
        });
        assert!(requests.try_recv().is_err());

        host.update(cx, |host, _| host.account.id = account);
        form.update(cx, |form, cx| form.open(cx));
        let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("launch") };
        assert!(matches!(queued.launch.target, Target::Bundle(path) if path == prepared));
    }

    #[gpui::test]
    fn preparation_admission_survives_closed_and_replaced_forms(cx: &mut gpui::TestAppContext) {
        let (host, _window, _requests) = test_support::host(cx);
        // Do not yield until after replacement: the scheduled preparation still owns
        // its credit. The invalid file scheme later fails locally, without engine IPC.
        let invalid = "https://example.com/App.pbapp";
        host.update(cx, |host, cx| {
            host.show_app_document(invalid.into(), cx);
            host.new_session = None;
            for _ in 0..32 {
                host.show_app_document(invalid.into(), cx);
                let form = host.new_session.as_ref().unwrap().read(cx);
                assert!(form.error.contains("already being prepared"));
                assert!(!form.prepared);
            }
            host.new_session = None;
            assert!(host.app_preparation.acquire().is_err());
        });
        cx.run_until_parked();
        host.update(cx, |host, cx| {
            assert!(
                host.app_preparation.acquire().is_ok(),
                "closed form must release on completion"
            );
            host.show_app_document(invalid.into(), cx);
            assert!(host.new_session.as_ref().unwrap().read(cx).error.is_empty());
        });
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert!(!host.new_session.as_ref().unwrap().read(cx).error.is_empty());
            assert!(host.app_preparation.acquire().is_ok(), "failed preparation must release");
        });
    }

    #[gpui::test]
    fn additional_window_form_keeps_its_owner_until_ready_or_late_failure(
        cx: &mut gpui::TestAppContext,
    ) {
        // Submission leaves both the form and row pending. A late callback
        // still has an owner after navigation, and only exact Ready succeeds.
        for succeeds in [false, true] {
            let (host, _, requests, responses) = test_support::host_with_responses(cx);
            let session = crate::identifier::encode(&[7; 32]);
            let receipt = guest_app::LaunchReceipt {
                incarnation: crate::store::Id::from_bytes([2; 16]),
                instance: crate::store::Id::from_bytes([1; 16]),
                owner_generation: 1,
                presentation: 0,
            };
            let form = host.update(cx, |host, cx| {
                let form =
                    cx.new(|_| Form::new(Target::Session(session.clone()), host.send.clone()));
                host.show_session_form(form.clone(), cx);
                form
            });
            form.update(cx, |form, cx| {
                form.open(cx);
                form.open(cx);
            });
            let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("launch") };
            assert!(requests.is_empty(), "duplicate form activation is suppressed");
            queued
                .reply
                .try_send(Completion {
                    result: Ok(Outcome::Opened(receipt)),
                    _credit: queued.credit,
                })
                .unwrap_or_else(|_| panic!("completion"));
            cx.run_until_parked();
            host.read_with(cx, |host, cx| {
                assert_eq!(host.launches[&session].instance, Some(receipt));
                assert!(host.launch_forms.contains_key(&form.entity_id()));
                assert!(form.read(cx).busy);
                assert!(!form.read(cx).opened);
            });
            host.update(cx, |host, cx| {
                host.prepare_session(session.clone(), cx).unwrap();
            });
            assert!(requests.is_empty(), "a retained link joins the form's admitted opening");
            host.update(cx, |host, _| host.new_session = None);
            let mut response =
                WorkerResponse::new(if succeeds { Ok(()) } else { Err("Launch rejected".into()) });
            if succeeds {
                response.startup = Some(vec![crate::engine::session_instance::StartupProgress {
                    incarnation: receipt.incarnation,
                    instance: receipt.instance,
                    owner_generation: receipt.owner_generation,
                    presentation: receipt.presentation,
                    session: session.clone(),
                    phase: crate::engine::session_instance::StartupPhase::Ready,
                }]);
            } else {
                response.launched = Some(receipt);
                response.launch_failed = true;
            }
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, cx| {
                assert!(!host.launches.contains_key(&session));
                assert!(!host.launch_forms.contains_key(&form.entity_id()));
                assert!(!form.read(cx).busy);
                assert_eq!(form.read(cx).opened, succeeds);
                if !succeeds {
                    assert_eq!(host.error, "Launch rejected");
                    assert_eq!(form.read(cx).error, "Launch rejected");
                }
            });
        }
    }

    #[gpui::test]
    fn session_list_limit_ends_form_progress_even_if_submission_later_succeeds(
        cx: &mut gpui::TestAppContext,
    ) {
        // A created session that cannot enter the bounded list still needs a
        // terminal UI owner; a late submission receipt cannot imply readiness.
        let (host, _, requests, _) = test_support::host_with_responses(cx);
        let form = host.update(cx, |host, cx| {
            host.sessions = (0..=u8::MAX)
                .map(|index| SessionRow {
                    reference: String::new(),
                    id: crate::identifier::encode(&[index; 32]),
                    resolved_build_id: crate::identifier::encode(&[1; 32]),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some("TODO".into()),
                })
                .collect();
            let form =
                cx.new(|_| Form::new(Target::Bundle("/tmp/TODO.pbapp".into()), host.send.clone()));
            host.show_session_form(form.clone(), cx);
            form
        });
        form.update(cx, |form, cx| form.open(cx));
        let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("launch") };
        host.update(cx, |host, cx| {
            host.session_created(
                queued.form,
                SessionRow {
                    reference: String::new(),
                    id: crate::identifier::encode(&[2, 3].repeat(16)),
                    resolved_build_id: crate::identifier::encode(&[1; 32]),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some("TODO".into()),
                },
                cx,
            )
        });
        queued
            .reply
            .try_send(Completion {
                result: Ok(Outcome::Opened(guest_app::LaunchReceipt {
                    incarnation: crate::store::Id::from_bytes([2; 16]),
                    instance: crate::store::Id::from_bytes([1; 16]),
                    owner_generation: 1,
                    presentation: 0,
                })),
                _credit: queued.credit,
            })
            .unwrap_or_else(|_| panic!("completion"));
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert_eq!(host.sessions.len(), 256);
            assert!(host.launch_forms.is_empty());
            assert!(!form.read(cx).busy);
            assert!(!form.read(cx).opened);
            assert!(form.read(cx).uncertain);
            assert!(form.read(cx).error.contains("display limit"));
        });
    }

    #[gpui::test]
    fn created_session_is_selected_before_launch_finishes(cx: &mut gpui::TestAppContext) {
        // Exercise selection while launch is pending and both terminal-reply delivery orders.
        for completed_first in [false, true] {
            let (host, window, requests, responses) = test_support::host_with_responses(cx);
            host.update(cx, |host, cx| {
                host.show_new_session(Target::Bundle("/tmp/TODO.pbapp".into()), cx);
            });
            let form = host.read_with(cx, |host, _| host.new_session.clone().unwrap());
            form.update(cx, |form, cx| form.open(cx));
            let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("launch") };
            let row = SessionRow {
                reference: String::new(),
                id: "ab".repeat(32),
                resolved_build_id: "cd".repeat(32),
                build_state: crate::engine::builds::BuildState::Ready,
                app_name: Some("TODO".into()),
            };
            let mut completion = Some(Completion {
                result: Err(Failure {
                    message: "Launch Services rejected the app".into(),
                    uncertain: true,
                }),
                _credit: queued.credit,
            });
            if completed_first {
                queued
                    .reply
                    .try_send(completion.take().unwrap())
                    .unwrap_or_else(|_| panic!("completion"));
                cx.run_until_parked();
            }
            let mut response = WorkerResponse::new(Ok(()));
            response.session_created = Some((queued.form, row.clone()));
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert_eq!(host.selected_session_id(), Some(row.id.as_str()));
                assert_eq!(host.sessions, vec![row]);
                assert!(host.new_session.is_none());
            });
            window
                .update(cx, |_, window, _| {
                    assert!(window.accessibility_error().is_none());
                    assert!(window.accessibility_nodes_for_test().any(|(_, node)| {
                        node.label() == Some("TODO") && node.is_selected() == Some(true)
                    }));
                })
                .unwrap();
            if let Some(completion) = completion {
                queued.reply.try_send(completion).unwrap_or_else(|_| panic!("completion"));
            }
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert_eq!(host.selected_session_id(), Some("ab".repeat(32).as_str()));
                assert!(host.error.contains("Launch Services rejected the app"));
            });
        }
    }

    #[gpui::test]
    fn delayed_creation_preserves_navigation_and_existing_session_row(
        cx: &mut gpui::TestAppContext,
    ) {
        // A status update and navigation may both arrive before the creation notice.
        let (host, _, requests, responses) = test_support::host_with_responses(cx);
        host.update(cx, |host, cx| {
            host.show_new_session(Target::Bundle("/tmp/TODO.pbapp".into()), cx);
        });
        let form = host.read_with(cx, |host, _| host.new_session.clone().unwrap());
        form.update(cx, |form, cx| form.open(cx));
        let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("launch") };
        let row = SessionRow {
            reference: String::new(),
            id: "ab".repeat(32),
            resolved_build_id: "cd".repeat(32),
            build_state: crate::engine::builds::BuildState::Ready,
            app_name: Some("TODO".into()),
        };
        host.update(cx, |host, cx| {
            host.sessions.push(row.clone());
            host.show_new_session(Target::Bundle("/tmp/another.pbapp".into()), cx);
        });
        let current = host.read_with(cx, |host, _| host.new_session.clone());
        let mut response = WorkerResponse::new(Ok(()));
        response.session_created = Some((queued.form, row));
        responses.try_send(response).unwrap();
        queued
            .reply
            .try_send(Completion {
                result: Ok(Outcome::Opened(guest_app::LaunchReceipt {
                    presentation: 0,
                    incarnation: crate::store::Id::from_bytes([2; 16]),
                    instance: crate::store::Id::from_bytes([1; 16]),
                    owner_generation: 1,
                })),
                _credit: queued.credit,
            })
            .unwrap_or_else(|_| panic!("completion"));
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.selected_session_id().is_none());
            assert_eq!(host.new_session, current);
            assert_eq!(host.sessions.len(), 1);
            assert_eq!(host.launches.len(), 1);
            assert!(host.launches.values().all(|progress| progress.instance.is_some()));
            assert!(host.launches.values().all(|progress| !progress.visible));
        });
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.launches.values().all(|progress| progress.visible));
        });
    }

    #[test]
    fn creation_notice_precedes_preparation_and_survives_preparation_failure() {
        // The engine rejects preparation only after observing the creation callback.
        use crate::engine::ipc::codec;
        use std::sync::atomic::{AtomicBool, Ordering};
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("control")).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(
            directory.path().join(crate::paths::ENGINE_SOCKET),
        )
        .unwrap();
        let bundle = directory.path().join("TODO.pbapp");
        let package = crate::package::Package::new(
            "test".into(),
            "TODO".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::new(),
        )
        .unwrap();
        std::fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
        let notified = Arc::new(AtomicBool::new(false));
        let observed = notified.clone();
        let server = std::thread::spawn(move || {
            let replies = [
                Reply::Developer(crate::engine::reply::DeveloperReply::Session(
                    crate::store::Session {
                        id: "ab".repeat(32),
                        selector: crate::store::BuildSelector::Exact { build_id: "cd".repeat(32) },
                        resolved_build_id: "cd".repeat(32),
                        root_volume: crate::store::Id::from_bytes([1; 16]),
                        shared_volume: None,
                        app: crate::package::AppIdentity {
                            publisher: crate::store::Id::from_bytes([3; 16]),
                            app_id: "test".into(),
                        },

                        creator: crate::store::Id::from_bytes([3; 16]),
                        update_authority: crate::store::UpdateAuthority::Creator,
                    },
                )),
                Reply::Error { error: "Preparation failed".into() },
            ];
            for (index, reply) in replies.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let request = ipc::read(&mut stream).unwrap();
                let request =
                    rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&request).unwrap();
                if index == 0 {
                    assert!(matches!(request, Request::CreateSession { .. }));
                } else {
                    assert!(matches!(request, Request::PrepareSession { .. }));
                    assert!(observed.load(Ordering::Acquire));
                }
                ipc::write(&mut stream, &codec::encode(&reply, 4096).unwrap()).unwrap();
            }
        });
        let failure = execute(
            directory.path(),
            Launch { target: Target::Bundle(bundle), mounts: vec![] },
            false,
            |row| {
                assert_eq!(row.id, "ab".repeat(32));
                assert_eq!(row.app_name.as_deref(), Some("TODO"));
                notified.store(true, Ordering::Release);
            },
            None,
        )
        .err()
        .unwrap();
        assert!(failure.uncertain);
        assert_eq!(failure.message, "Preparation failed");
        server.join().unwrap();
    }

    #[gpui::test]
    fn folder_drafts_are_read_only_and_only_open_transfers_permission(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, requests, _) = worker_queue::channel();
        let form = cx.new(|_| {
            Form::new(Target::Session(crate::identifier::encode(&[0xab; 32])), send.clone())
        });
        form.update(cx, |form, cx| {
            form.add(PathBuf::from("/tmp/Café"), cx);
            assert!(!form.rows[0].write);
            assert_eq!(form.rows[0].label.read(cx).text(), "Café");
            assert!(requests.try_recv().is_err());
            form.open(cx);
            form.open(cx);
            assert!(form.busy);
        });
        let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("launch") };
        assert_eq!(queued.launch.mounts.len(), 1);
        assert!(!queued.launch.mounts[0].write);
        assert_eq!(queued.launch.mounts[0].label, "Café");
        assert!(requests.try_recv().is_err(), "an accepted launch cannot be repeated");
        // The launch retains a shared management credit through UI delivery.
        for _ in 0..31 {
            send.send(Request::Status).unwrap();
        }
        assert!(send.send(Request::Status).is_err());
        queued
            .reply
            .try_send(Completion {
                result: Err(Failure { message: "Lost launch reply".into(), uncertain: true }),
                _credit: queued.credit,
            })
            .unwrap_or_else(|_| panic!("completion"));
        cx.run_until_parked();
        form.update(cx, |form, cx| {
            assert!(form.uncertain);
            assert!(!form.busy);
            form.open(cx);
        });
        while let Ok(message) = requests.try_recv() {
            assert!(matches!(message, Message::Request(_)), "uncertain launch was replayed");
        }
        send.send(Request::Status).unwrap();
    }

    #[gpui::test]
    fn failed_folder_load_can_be_retried_without_admitting_a_launch(cx: &mut gpui::TestAppContext) {
        let (send, requests, _) = worker_queue::channel();
        let form = cx.new(|cx| Form::folders(crate::identifier::encode(&[0xab; 32]), send, cx));
        let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("load") };
        assert!(matches!(queued.launch.target, Target::Folders { .. }));
        queued
            .reply
            .try_send(Completion {
                result: Err(Failure { message: "Disconnected".into(), uncertain: false }),
                _credit: queued.credit,
            })
            .unwrap_or_else(|_| panic!("completion"));
        cx.run_until_parked();
        form.update(cx, |form, cx| {
            assert!(!form.busy);
            assert_eq!(form.error, "Disconnected");
            form.reload(cx);
            assert!(form.busy);
        });
        let Message::Launch(retry) = requests.try_recv().unwrap() else { panic!("retry") };
        assert!(matches!(retry.launch.target, Target::Folders { .. }));
        assert!(retry.launch.mounts.is_empty());
    }
    #[test]
    fn error_after_saving_folders_remains_uncertain_until_reload() {
        use crate::engine::{
            ipc::codec,
            session_instance::{SessionInstance, State},
        };
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("control")).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(
            directory.path().join(crate::paths::ENGINE_SOCKET),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = ipc::read(&mut stream).unwrap();
            ipc::write(
                &mut stream,
                &codec::encode(
                    &Reply::Error { error: "Could not confirm durable directory write".into() },
                    4096,
                )
                .unwrap(),
            )
            .unwrap();
        });
        let instance = SessionInstance {
            id: crate::store::Id::from_bytes([1; 16]),
            launch_request: crate::store::Id::from_bytes([2; 16]),
            account: crate::store::Id::from_bytes([4; 16]),
            session: crate::identifier::encode(&[0xab; 32]),
            build: crate::identifier::encode(&[0xcd; 32]),
            root: crate::store::Id::from_bytes([3; 16]),

            extra_volumes: vec![],
            host_mounts: Box::default(),
            restore_host_mounts: true,
            owner_generation: 1,
            state: State::Owned,
        };
        let result = execute(
            directory.path(),
            Launch {
                target: Target::Configuration(Box::new(Configuration {
                    instance,
                    index: 0,
                    count: 1,
                })),
                mounts: vec![],
            },
            false,
            |_| panic!("Saving folders does not create a session"),
            None,
        );
        let Err(failure) = result else { panic!("uncertain save error") };
        assert!(failure.uncertain);
        server.join().unwrap();
    }

    #[test]
    fn invalid_mount_fails_before_creating_a_session() {
        let directory = tempfile::tempdir().unwrap();
        let launch = Launch {
            target: Target::Bundle(directory.path().join("missing.pbapp")),
            mounts: vec![Mount {
                original: None,
                path: directory.path().join("missing-folder"),
                label: "Documents".into(),
                write: false,
            }],
        };
        let error = execute(
            directory.path(),
            launch,
            false,
            |_| panic!("Invalid mounts must fail before session creation"),
            None,
        )
        .err()
        .unwrap();
        assert!(!error.uncertain);
        assert!(error.message.contains("Cannot open host mount"));
    }

    #[test]
    fn folder_selection_rejects_equivalent_directories_and_allows_self_replacement() {
        // Canonical paths detect symlink aliases for adding and replacing; self-replacement is valid.
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("folder");
        let other = directory.path().join("other");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&folder).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&folder, &alias).unwrap();
        let existing = [(1, folder.clone()), (2, other.clone())];
        for path in [folder.clone(), alias.clone(), folder.join(".")] {
            assert!(selected_folder(path.clone(), &existing, None).is_err());
            assert!(selected_folder(path.clone(), &existing, Some(2)).is_err());
            assert_eq!(selected_folder(path.clone(), &existing, Some(1)).unwrap(), path);
        }
        let unavailable = [(3, directory.path().join("missing"))];
        assert_eq!(selected_folder(other.clone(), &unavailable, None).unwrap(), other);
        assert!(selected_folder(directory.path().join("missing"), &[], None).is_err());
        let file = directory.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        assert!(selected_folder(file, &[], None).is_err());
    }

    #[gpui::test]
    fn dismissing_launch_review_restores_the_selected_session(cx: &mut gpui::TestAppContext) {
        // Canceling either kind of launch returns to the selected session.
        let (host, window, _requests) = test_support::host(cx);
        let session = crate::identifier::encode(&[4; 32]);
        host.update(cx, |host, _| {
            host.sessions.push(SessionRow {
                reference: String::new(),
                id: session.clone(),
                resolved_build_id: crate::identifier::encode(&[5; 32]),
                build_state: crate::engine::builds::BuildState::Ready,
                app_name: Some("TODO".into()),
            });
            host.sidebar_session = Some(session.clone());
        });
        for target in [Target::Bundle("/tmp/TODO.pbapp".into()), Target::Session(session)] {
            host.update(cx, |host, cx| {
                let form = cx.new(|_| Form::new(target, host.send.clone()));
                host.show_session_form(form, cx);
            });
            cx.run_until_parked();
            window
                .update(cx, |host, window, cx| {
                    assert!(host.session_dialog_open(cx));
                    let form = host.new_session.as_ref().unwrap().read(cx);
                    form.controls[&(0, 6)].read(cx).focus(window);
                })
                .unwrap();
            test_support::press(window, "enter", cx);
            window
                .update(cx, |host, window, _| {
                    assert!(host.new_session.is_none());
                    assert!(
                        window
                            .accessibility_nodes_for_test()
                            .any(|(_, node)| node.label() == Some("TODO")
                                && node.is_selected() == Some(true))
                    );
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn permission_popup_preserves_editor_width_and_submits_only_on_start(
        cx: &mut gpui::TestAppContext,
    ) {
        // Selecting a permission updates the draft and preserves the label editor's width.
        let (host, window, requests) = test_support::host(cx);
        let form = host.update(cx, |host, cx| {
            let form = cx.new(|_| {
                Form::new(Target::Session(crate::identifier::encode(&[2; 32])), host.send.clone())
            });
            form.update(cx, |form, cx| form.add("/tmp/Café".into(), cx));
            host.show_session_form(form.clone(), cx);
            form
        });
        cx.run_until_parked();
        let bounds = window
            .update(cx, |_, window, _| {
                window
                    .accessibility_nodes_for_test()
                    .find_map(|(_, node)| {
                        (node.role() == gpui::accesskit::Role::TextInput
                            && node.placeholder() == Some("Folder label"))
                        .then(|| node.bounds().unwrap())
                    })
                    .unwrap()
            })
            .unwrap();
        window
            .update(cx, |_, _, cx| {
                let popup = form.read(cx).controls[&(1, 1)].read(cx);
                let (items, _, choose) = popup.popup_for_test();
                assert!(matches!(
                    &items[0],
                    crate::runtime::MenuEntry::Action { checked: Some(true), .. }
                ));
                choose(1, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(form.read_with(cx, |form, _| form.rows[0].write));
        assert!(requests.try_recv().is_err());
        window
            .update(cx, |_, window, _| {
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.role() == gpui::accesskit::Role::ComboBox
                            && node.label() == Some("Read + Write"))
                );
                let changed = window
                    .accessibility_nodes_for_test()
                    .find_map(|(_, node)| {
                        (node.role() == gpui::accesskit::Role::TextInput
                            && node.placeholder() == Some("Folder label"))
                        .then(|| node.bounds().unwrap())
                    })
                    .unwrap();
                assert_eq!(bounds.x1 - bounds.x0, changed.x1 - changed.x0);
            })
            .unwrap();
        form.update(cx, |form, cx| {
            form.add("/tmp/Second".into(), cx);
            cx.notify();
        });
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| form.read(cx).controls[&(2, 2)].read(cx).focus(window))
            .unwrap();
        test_support::press(window, "enter", cx);
        window
            .update(cx, |_, window, cx| {
                let draft = form.read(cx);
                assert_eq!(draft.rows.len(), 1);
                assert!(draft.controls[&(0, 1)].read(cx).focused(window));
            })
            .unwrap();
        form.update(cx, |form, cx| form.open(cx));
        let Message::Launch(queued) = requests.try_recv().unwrap() else { panic!("launch") };
        assert_eq!(queued.launch.mounts.len(), 1);
        assert!(queued.launch.mounts[0].write);
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.role() == gpui::accesskit::Role::Button
                            && node.label() == Some("Start Session"))
                );
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.live() == Some(gpui::accesskit::Live::Polite)
                            && node.value().is_some())
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn launch_actions_share_a_footer_when_resizing(cx: &mut gpui::TestAppContext) {
        // Measure actual controls across the width cap, UI scales, and folder drafts.
        use crate::ui::desktop::zoom;
        cx.update(zoom::install);
        let (host, window, _requests) = test_support::host(cx);
        let form = host.update(cx, |host, cx| {
            let form =
                cx.new(|_| Form::new(Target::Bundle("/tmp/TODO.pbapp".into()), host.send.clone()));
            host.show_session_form(form.clone(), cx);
            form
        });
        for folders in 0..=1 {
            if folders == 1 {
                form.update(cx, |form, cx| form.add("/tmp/Documents".into(), cx));
            }
            for units in zoom::SAMPLES {
                let scale = units as f32 / 10_000.;
                cx.update(|cx| zoom::set_scale(units, cx).expect("supported scale"));
                for width in [500., 650., 800., 900., 1024., 1200., 1600., 900., 500.] {
                    let width = (width * scale).max(500.);
                    cx.simulate_window_resize(
                        window.into(),
                        size(px(width), px(900. * scale.max(1.))),
                    );
                    cx.run_until_parked();
                    window
                        .update(cx, |_, window, _| {
                            let button = |label| {
                                window
                                    .accessibility_nodes_for_test()
                                    .find_map(|(_, node)| {
                                        (node.role() == gpui::accesskit::Role::Button
                                            && node.label() == Some(label))
                                        .then(|| node.bounds().expect("button bounds"))
                                    })
                                    .expect("rendered button")
                            };
                            let cancel = button("Cancel");
                            let start = button("Start Session");
                            let add = button("Add folder…");
                            assert!((cancel.y0 - start.y0).abs() <= 1.);
                            assert!(cancel.x1 + f64::from(8. * scale) - 1. <= start.x0);
                            assert!(add.y1 <= start.y0, "overlap at {width}, {scale}, {folders}");
                        })
                        .unwrap();
                }
            }
        }
    }

    #[gpui::test]
    fn long_folder_form_uses_the_visible_scrollbar_viewport(cx: &mut gpui::TestAppContext) {
        // A maximum-size draft scrolls through the shared handle exposed by its scrollbar.
        let (host, window, _requests) = test_support::host(cx);
        let form = host.update(cx, |host, cx| {
            let form = cx.new(|_| {
                Form::new(Target::Session(crate::identifier::encode(&[2; 32])), host.send.clone())
            });
            form.update(cx, |form, cx| {
                for index in 0..MAX_MOUNTS {
                    form.add(format!("/tmp/folder-{index}").into(), cx);
                }
            });
            host.show_session_form(form.clone(), cx);
            form
        });
        cx.run_until_parked();
        let handle =
            form.read_with(cx, |form, cx| form.scrollbars.as_ref().unwrap().read(cx).handle());
        assert!(handle.max_offset().height > px(0.));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: handle.bounds().center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-40.))),
            ..Default::default()
        });
        assert_eq!(handle.offset().y, px(-40.));
        window
            .update(&mut visual, |_, window, _| {
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.role() == gpui::accesskit::Role::ScrollBar)
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn form_exposes_folder_labels_permissions_and_errors_to_accessibility(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            let form = cx.new(|_| {
                Form::new(
                    Target::Session(crate::identifier::encode(&[0xab; 32])),
                    host.send.clone(),
                )
            });
            form.update(cx, |form, cx| {
                form.add(PathBuf::from("/tmp/Café"), cx);
                form.error = "Choose unique folder labels.".into();
            });
            // A folder form opened from a session's menu must also replace the trace pane.
            host.sidebar_session = Some(crate::identifier::encode(&[0xab; 32]));
            host.new_session = Some(form);
            cx.notify();
        });
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                use gpui::accesskit::{Live, Role};
                assert!(window.accessibility_error().is_none());
                let nodes =
                    window.accessibility_nodes_for_test().map(|(_, node)| node).collect::<Vec<_>>();
                assert!(nodes.iter().any(
                    |node| node.role() == Role::Heading && node.label() == Some("New Session")
                ));
                assert!(nodes.iter().any(|node| node.value() == Some("/tmp/Café")));
                assert!(nodes.iter().any(|node| node.label() == Some("Remove folder")));
                assert!(
                    nodes
                        .iter()
                        .any(|node| node.role() == Role::ComboBox
                            && node.label() == Some("Read only"))
                );
                assert!(
                    nodes.iter().any(|node| node.value() == Some("Choose unique folder labels.")
                        && node.live() == Some(Live::Polite))
                );
            })
            .unwrap();
        assert!(requests.try_recv().is_err());
        test_support::check_zoom_geometry(window.into(), cx);
    }
}
