//! Capture on the failing owner; resolve symbols only on the diagnostic worker.
use std::{backtrace::Backtrace, fmt, sync::Arc};

#[derive(Clone, Debug)]
pub(crate) struct Origin(Arc<Backtrace>);

impl Origin {
    pub(crate) fn capture() -> Self {
        Self(Arc::new(Backtrace::force_capture()))
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Error origin backtrace:")?;
        fmt::Display::fmt(self.0.as_ref(), f)
    }
}
