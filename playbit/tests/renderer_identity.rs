#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use gpui::{Element, ElementId, GlobalElementId};
use std::{collections::HashMap, mem::size_of};

#[test]
fn identity_paths_keep_small_element_state_and_preserve_deep_ids() {
    let bytes = size_of::<GlobalElementId>();
    eprintln!(
        "global identity bytes: {bytes}; div request bytes: {}; div prepaint bytes: {}",
        size_of::<<gpui::Div as Element>::RequestLayoutState>(),
        size_of::<<gpui::Div as Element>::PrepaintState>()
    );
    assert!(bytes <= 512, "identity storage exceeds 512 bytes: {bytes}");
    for depth in [0, 1, 8, 9, 32, 64] {
        let mut path = GlobalElementId::default();
        path.extend((0..depth).map(ElementId::Integer));
        let mut copy = GlobalElementId::default();
        copy.extend(path.iter().cloned());
        assert_eq!(path, copy);
        assert_eq!(path.len(), depth as usize);
        assert_eq!(path.spilled(), depth > 8);
        assert_eq!(copy.spilled(), depth > 8);
        for (index, id) in path.iter().enumerate() {
            assert_eq!(*id, ElementId::Integer(index as u64));
        }
        let mut states = HashMap::new();
        states.insert(path, depth);
        assert_eq!(states.get(&copy), Some(&depth));
        copy.push(ElementId::Integer(depth));
        assert_eq!(states.get(&copy), None);
        copy.pop();
        assert_eq!(states.get(&copy), Some(&depth));
    }
}
