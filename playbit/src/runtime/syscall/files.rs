//! File operations and scene-bound file authority checks.
use super::{Host, HostError, HostResult, Opcode, State, VolumeRights};

pub(in crate::runtime) fn file_descriptor(
    host: &mut dyn Host,
    address: super::EntryAddress,
) -> HostResult<(u64, [u8; 32])> {
    host.file_metadata(address)
}

// fuzz: wasm_operations
pub(super) fn validate_scene_capabilities(
    state: &State,
    view: &crate::runtime::ViewNode,
) -> HostResult<()> {
    use crate::runtime::ViewNode;
    match view {
        ViewNode::Canvas { items, viewport, .. } => {
            if *viewport
                && !state
                    .host
                    .supported_features()
                    .contains(&playbit_api::ui::CANVAS_VIEWPORT_FEATURE)
            {
                return Err(HostError::Unsupported);
            }
            if items.iter().any(|item| matches!(item, crate::runtime::CanvasItem::Pixels { .. }))
                && !state
                    .host
                    .supported_features()
                    .contains(&playbit_api::ui::CANVAS_PIXELS_FEATURE)
            {
                return Err(HostError::Unsupported);
            }
            Ok(())
        }
        ViewNode::FileImage { file, .. } => {
            if !state.supported.contains(&Opcode::FileReadBegin)
                || !state.supported.contains(&Opcode::FileReadTake)
            {
                return Err(HostError::Unsupported);
            }
            super::file_ops::address(state, *file, VolumeRights::READ).map(|_| ())
        }
        ViewNode::Theme { child, .. }
        | ViewNode::ContextMenu { child, .. }
        | ViewNode::Scroll { child, .. } => validate_scene_capabilities(state, child),
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            for child in children {
                validate_scene_capabilities(state, child)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
