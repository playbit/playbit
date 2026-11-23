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
    use playbit_api::ui::{
        CANVAS_CLEAR_FEATURE, CANVAS_PIXELS_FEATURE, CANVAS_VIEWPORT_FEATURE,
        CANVAS_WINDOW_FEATURE, CanvasExtent, WINDOW_SIZE_CONSTRAINTS_FEATURE, WINDOW_STYLE_FEATURE,
    };
    match view {
        ViewNode::Canvas { items, extent, clear_color, .. } => {
            let supported = state.host.supported_features();
            let extent_supported = match extent {
                CanvasExtent::Inline => true,
                CanvasExtent::Viewport => supported.contains(&CANVAS_VIEWPORT_FEATURE),
                CanvasExtent::Window => supported.contains(&CANVAS_WINDOW_FEATURE),
            };
            if !extent_supported
                || (clear_color.is_some() && !supported.contains(&CANVAS_CLEAR_FEATURE))
            {
                return Err(HostError::Unsupported);
            }
            if items.iter().any(|item| matches!(item, crate::runtime::CanvasItem::Pixels { .. }))
                && !supported.contains(&CANVAS_PIXELS_FEATURE)
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
        ViewNode::Theme { child, window_style, window_size_constraints, .. } => {
            let supported = state.host.supported_features();
            if (window_style.is_some() && !supported.contains(&WINDOW_STYLE_FEATURE))
                || (window_size_constraints.is_some()
                    && !supported.contains(&WINDOW_SIZE_CONSTRAINTS_FEATURE))
            {
                return Err(HostError::Unsupported);
            }
            validate_scene_capabilities(state, child)
        }
        ViewNode::ContextMenu { child, .. } | ViewNode::Scroll { child, .. } => {
            validate_scene_capabilities(state, child)
        }
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            for child in children {
                validate_scene_capabilities(state, child)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
