//! Desktop application entry points and components.
mod account_menu;
mod agent_skill;
mod ap_status;
mod application_browser;
mod browser;
mod crash_reports;
mod draft_lifecycle;
mod draft_publications;
mod drafts;
mod entry;
mod feedback;
pub use entry::app_main;
pub(crate) use feedback::show_error_feedback;
mod file_document;
#[cfg(target_os = "macos")]
pub mod file_provider;
pub mod guest_app;
mod guest_runtime;
mod host_text;
mod host_updates;
mod invitation_view;
mod join_dialog;
mod launch_progress;
pub(crate) mod launch_timing;
pub use launch_timing::process_entry as record_process_entry;
mod log;
#[cfg(not(test))]
mod macos;
mod menus;
mod new_session;
mod personal;
pub mod process_log;
mod release_channel;
mod session;
mod session_dialog;
mod session_icons;
mod session_log_view;
mod session_view;
mod shell_controls;
mod shell_view;
mod sidebar_menu;
#[cfg(target_os = "macos")]
mod sidebar_view;
mod sign_in;
mod sign_out;
mod startup;
mod startup_failure;
mod status_bar;
#[cfg(feature = "desktop-qualification")]
mod status_bar_preview;
#[cfg(test)]
pub(crate) mod test_support;
mod theme;
#[cfg(target_os = "macos")]
pub mod update;
mod volume_creation;
mod welcome_view;
mod worker;
mod worker_copy;
mod worker_data;
mod worker_queue;
mod worker_recovery;
mod worker_refresh;
mod worker_response;
mod worker_watch;

use crate::engine::{
    Request, ipc, profile_path,
    reply::{
        AccountProfile, InvitationState, InvitationStatus, RecoveryStatus, Reply, SignInReply,
        TransferKind, TransferStatus,
    },
};
use crate::ui::desktop::sign_in::validation as account_form;
use crate::ui::desktop::{UI_FONT_FAMILY, controls, text_editor};
use gpui::{
    App, Application, Bounds, ClipboardItem, Context, Entity, PromptButton, PromptLevel,
    SharedString, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, rgba, size,
};
use host_text::Text as HostText;
use session_log_view::LogView;
use shell_view::{Shell, maybe_send_crash_reports};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};
#[cfg(test)]
use test_support::{focus_button, host, press};
use theme::theme;
use worker_queue::{Message, QueuedRequest, RequestSender};
use worker_response::{AccountResponse, SessionRow, WorkerResponse};

pub fn main() {
    startup::main_with_focus(true);
}

/// Isolated, synthetic status snapshots in the actual Sessions shell.
#[cfg(feature = "desktop-qualification")]
#[doc(hidden)]
pub fn main_status_bar_preview() {
    status_bar_preview::main();
}

/// Isolated sidebar states using the actual native shell.
#[cfg(feature = "desktop-qualification")]
#[doc(hidden)]
pub fn main_sidebar_preview(icons: Option<std::path::PathBuf>, session: Option<String>) {
    status_bar_preview::sidebar(icons, session);
}

/// Isolated updater dialogs and sidebar controls; no downloads or installation.
#[cfg(all(target_os = "macos", feature = "desktop-qualification"))]
#[doc(hidden)]
pub fn main_update_preview() {
    status_bar_preview::updates();
}
