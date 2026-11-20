//! Platform-independent UI building blocks shared by the native hosts.
#[cfg(feature = "app-host")]
pub mod app_control;
#[cfg(any(feature = "native-ui", feature = "app-host"))]
pub mod collation;
#[cfg(feature = "native-ui")]
pub mod text;

#[cfg(feature = "desktop-ui")]
pub mod desktop;

#[cfg(feature = "app-host")]
pub(crate) mod local_guest;

#[cfg(feature = "app-host")]
pub mod embedded;
