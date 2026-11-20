//! Borrowed typed instance metadata and bounded resource access.
use super::{
    AvailableVolume, HostResult, Identity, Opcode, Participant, State, VolumeHandle,
    require_supported,
};
use std::collections::BTreeMap;

pub(in crate::runtime) fn random_fill(state: &State, output: &mut [u8]) -> HostResult<()> {
    require_supported(state, Opcode::RandomFill)?;
    fill_random(output, |bytes| getrandom::fill(bytes).map_err(|_| super::HostError::Unavailable))
}

// Keep the failure path independently testable: a failed platform fill may have
// written a prefix. Never return those partial bytes as usable randomness.
fn fill_random(
    output: &mut [u8],
    fill: impl FnOnce(&mut [u8]) -> HostResult<()>,
) -> HostResult<()> {
    if output.len() > playbit_api::abi::syscall::MAX_RANDOM_BYTES {
        return Err(super::HostError::LimitExceeded);
    }
    if output.is_empty() {
        return Ok(());
    }
    let result = fill(output);
    if result.is_err() {
        output.fill(0);
    }
    result
}

pub(in crate::runtime) fn capabilities(state: &State) -> HostResult<&[String]> {
    require_supported(state, Opcode::Capabilities)?;
    Ok(&state.capability_names)
}
pub(in crate::runtime) fn identity(state: &State) -> HostResult<(u64, &Identity)> {
    require_supported(state, Opcode::Identity)?;
    Ok((u64::from(state.generation), &state.context.identity))
}
pub(in crate::runtime) fn presence(state: &State) -> HostResult<&[Participant]> {
    require_supported(state, Opcode::Presence)?;
    Ok(&state.context.participants)
}
pub(in crate::runtime) fn startup_arguments(
    state: &State,
) -> HostResult<&BTreeMap<String, String>> {
    require_supported(state, Opcode::StartupArguments)?;
    Ok(&state.context.startup_arguments)
}
pub(in crate::runtime) fn available_volumes(
    state: &State,
) -> HostResult<impl ExactSizeIterator<Item = (VolumeHandle, &AvailableVolume)>> {
    require_supported(state, Opcode::AvailableVolumes)?;
    assert!(state.context.volumes.len() <= state.volume_handles.len());
    Ok(state.context.volumes.iter().enumerate().map(|(index, volume)| {
        (super::handles::volume_handle(state.volume_handles[index]), volume)
    }))
}

/// The runtime is compiled for its host; the app may be portable Wasm code.
pub(in crate::runtime) fn host_platform(state: &State) -> HostResult<i32> {
    use playbit_api::abi::environment::HostPlatform;
    require_supported(state, Opcode::HostPlatform)?;
    let platform = if cfg!(target_os = "macos") {
        HostPlatform::MacOs
    } else if cfg!(target_os = "ios") {
        HostPlatform::Ios
    } else if cfg!(target_arch = "wasm32") {
        HostPlatform::Web
    } else {
        HostPlatform::Other
    };
    Ok(platform as i32)
}

pub(in crate::runtime) fn host_limit(state: &State, kind: u32) -> HostResult<i32> {
    require_supported(state, Opcode::HostLimit)?;
    if kind == playbit_api::abi::environment::HostLimit::ViewBytes as u32 {
        return Ok(state.view_bytes_limit as i32);
    }
    state.query_limits.get(kind)
}

#[cfg(test)]
mod random_tests {
    use super::*;

    #[test]
    fn random_fill_erases_partial_entropy_and_skips_empty_or_oversized_requests() {
        // Inject a partial platform failure and verify bounded, fail-closed output.
        let mut bytes = [42; 32];
        assert_eq!(
            fill_random(&mut bytes, |bytes| {
                bytes[..8].fill(7);
                Err(super::super::HostError::Unavailable)
            }),
            Err(super::super::HostError::Unavailable)
        );
        assert_eq!(bytes, [0; 32]);
        assert_eq!(fill_random(&mut [], |_| panic!("empty request must not use entropy")), Ok(()));
        let mut large = vec![42; playbit_api::abi::syscall::MAX_RANDOM_BYTES + 1];
        assert_eq!(
            fill_random(&mut large, |_| panic!("oversized request must not use entropy")),
            Err(super::super::HostError::LimitExceeded)
        );
        assert!(large.iter().all(|&byte| byte == 42));
    }
}
