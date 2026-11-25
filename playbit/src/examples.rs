//! A deliberately small, bundled list. Opening an example uses normal invitation
//! preview, enrollment, authorization and consent; it grants no special access.
use serde::Serialize;

#[derive(Clone, Copy, Serialize)]
pub struct Example {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub invitation: &'static str,
    pub source: &'static str,
}

pub const SHARED_TODO: Example = Example {
    id: "shared-todo",
    name: "Shared TODO",
    description: "Try adding and completing a task. You and other beta testers share this list; everyone can see and edit its tasks. Keep personal information out of it.",
    invitation: "https://playbit.app/s#RDJVpDon9tg5g3s4dkADuL",
    source: "https://github.com/playbit/playbit/tree/v0.1.3/guests/todo",
};

pub const EXAMPLES: &[Example] = &[SHARED_TODO];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn bundled_examples_fit_status_and_use_normal_invitations() {
        assert!(!EXAMPLES.is_empty() && EXAMPLES.len() <= 4);
        assert!(serde_json::to_vec(EXAMPLES).unwrap().len() <= 4096);
        for example in EXAMPLES {
            assert!(!example.name.is_empty() && example.name.len() <= 64);
            assert!(example.description.len() <= 512);
            crate::store::parse_session_link(
                example.invitation,
                crate::release_channel::ReleaseChannel::Public,
            )
            .unwrap();
            assert!(example.source.starts_with("https://github.com/"));
        }
        assert_eq!(
            SHARED_TODO.source,
            "https://github.com/playbit/playbit/tree/v0.1.3/guests/todo"
        );
    }
}
