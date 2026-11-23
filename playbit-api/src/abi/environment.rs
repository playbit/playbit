//! Host platform, caller-owned instance metadata and bounded resource results.
use super::data::Range;
use super::operations::Context;

/// The host running the app, independent of the app compiler's target.
/// Web identifies the web host, including browsers on macOS and iOS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum HostPlatform {
    Other = 0,
    MacOs = 1,
    Ios = 2,
    Web = 3,
}
impl TryFrom<i32> for HostPlatform {
    type Error = i32;
    fn try_from(value: i32) -> Result<Self, i32> {
        match value {
            0 => Ok(Self::Other),
            1 => Ok(Self::MacOs),
            2 => Ok(Self::Ios),
            3 => Ok(Self::Web),
            other => Err(other),
        }
    }
}

/// An instance's fixed resource ceilings, independent of host platform.
/// Discovery does not imply operation support; use capability discovery for that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum HostLimit {
    /// Maximum requested records per query page, including IDs-only queries.
    QueryRecords = 1,
    /// Aggregate logical value bytes per query page: text/bytes lengths,
    /// bool=1, integer/float=8, reference=32, file metadata=40. Record framing,
    /// scratch and app allocations are separate; queries may return fewer rows.
    QueryValueBytes = 2,
    /// Maximum encoded bytes for one ViewSubmit, including scene framing.
    /// This is not a node, text, renderer-feature or total-memory guarantee.
    ViewBytes = 3,
}
/// # Safety
/// Context obeys the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_host_limit(context: *mut Context, limit: u32) -> i64 {
    // SAFETY: the caller supplies a live context; all remaining arguments are scalars.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::HostLimit as u32,
            [limit as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// Available area in whole logical units. The default excludes host chrome and
/// padding. A root canvas with Window extent receives the full drawable window.
/// Scale is physical pixels per logical unit. Zero dimensions mean no visible area.
/// SurfaceChanged events (target zero) contain these four little-endian words;
/// pixel_scale uses IEEE 754 binary32 bits. Input bits describe supported delivery.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(Rust)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub pixel_scale: f32,
    pub inputs: u32,
}
impl Surface {
    pub const POINTER: u32 = 1;
    pub const KEYBOARD: u32 = 2;
    pub const TOUCH: u32 = 4;
    pub fn decode(bytes: &[u8]) -> Result<Self, super::syscall::Error> {
        let bytes: &[u8; 16] =
            bytes.try_into().map_err(|_| super::syscall::Error::InvalidArgument)?;
        let word =
            |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"));
        let surface = Self {
            width: word(0),
            height: word(4),
            pixel_scale: f32::from_bits(word(8)),
            inputs: word(12),
        };
        surface.validate()?;
        Ok(surface)
    }
    pub fn encode(self) -> Result<[u8; 16], super::syscall::Error> {
        self.validate()?;
        let mut bytes = [0; 16];
        for (part, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip([
            self.width,
            self.height,
            self.pixel_scale.to_bits(),
            self.inputs,
        ]) {
            *part = word.to_le_bytes();
        }
        Ok(bytes)
    }
    fn validate(self) -> Result<(), super::syscall::Error> {
        if !self.pixel_scale.is_finite() || self.pixel_scale <= 0.0 || self.inputs & !7 != 0 {
            return Err(super::syscall::Error::InvalidArgument);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Capabilities {
    pub size: u32,
    pub written: u32,
    pub names_data: *mut Range,
    pub names_capacity: u32,
    pub names_count: u32,
    pub names_stride: u32,
    pub bytes_data: *mut u8,
    pub bytes_capacity: u32,
    pub bytes_count: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Argument {
    pub size: u32,
    pub written: u32,
    pub name_offset: u32,
    pub name_count: u32,
    pub value_offset: u32,
    pub value_count: u32,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Arguments {
    pub size: u32,
    pub written: u32,
    pub arguments_data: *mut Argument,
    pub arguments_capacity: u32,
    pub arguments_count: u32,
    pub arguments_stride: u32,
    pub bytes_data: *mut u8,
    pub bytes_capacity: u32,
    pub bytes_count: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Participant {
    pub size: u32,
    pub written: u32,
    pub account_id: [u8; 16],
    pub device_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub build_id: [u8; 32],
}
/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_host_platform(context: *mut Context) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::HostPlatform as u32,
            [0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_capabilities(context: *mut Context, output: *mut Capabilities) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::Capabilities as u32,
            [output as usize as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_startup_arguments(context: *mut Context, output: *mut Arguments) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::StartupArguments as u32,
            [output as usize as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_presence(
    context: *mut Context,
    data: *mut Participant,
    capacity: u32,
    stride: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::Presence as u32,
            [data as usize as u64, capacity as u64, stride as u64, count as usize as u64, 0, 0],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::HostPlatform;
    #[test]
    fn platform_results_keep_errors_and_unknown_values_out_of_the_enum() {
        for (raw, expected) in [
            (0, HostPlatform::Other),
            (1, HostPlatform::MacOs),
            (2, HostPlatform::Ios),
            (3, HostPlatform::Web),
        ] {
            assert_eq!(HostPlatform::try_from(raw), Ok(expected));
        }
        for raw in [-11, -7, -1, 4, 5, i32::MIN, i32::MAX] {
            assert_eq!(HostPlatform::try_from(raw), Err(raw));
        }
    }
}

#[cfg(test)]
mod surface_tests {
    use super::Surface;
    #[test]
    fn surface_payload_has_one_exact_layout_and_rejects_invalid_scale_or_inputs() {
        let surface = Surface { width: 368, height: 448, pixel_scale: 2.0, inputs: Surface::TOUCH };
        let bytes = surface.encode().expect("valid surface");
        assert_eq!(bytes, [112, 1, 0, 0, 192, 1, 0, 0, 0, 0, 0, 64, 4, 0, 0, 0]);
        assert_eq!(Surface::decode(&bytes), Ok(surface));
        assert!(Surface::decode(&bytes[..15]).is_err());
        assert!(Surface::decode(&[0; 17]).is_err());
        for scale in [0.0, -0.0, -1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            let invalid = Surface { pixel_scale: scale, ..surface };
            assert!(invalid.encode().is_err());
            let mut bytes = bytes;
            bytes[8..12].copy_from_slice(&scale.to_bits().to_le_bytes());
            assert!(Surface::decode(&bytes).is_err());
        }
        assert!(Surface { inputs: 8, ..surface }.encode().is_err());
        assert!(Surface { width: 0, height: 0, inputs: 0, ..surface }.encode().is_ok());
    }
}

super::record::record!(Capabilities, if cfg!(target_pointer_width = "64") { 48 } else { 36 }, { names_data: *mut Range, names_capacity: u32, names_count: u32, names_stride: u32, bytes_data: *mut u8, bytes_capacity: u32, bytes_count: u32 });

impl Default for Argument {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            name_offset: 0,
            name_count: 0,
            value_offset: 0,
            value_count: 0,
        }
    }
}

super::record::record!(Argument, 24, { name_offset: u32, name_count: u32, value_offset: u32, value_count: u32 });

super::record::record!(Arguments, if cfg!(target_pointer_width = "64") { 48 } else { 36 }, { arguments_data: *mut Argument, arguments_capacity: u32, arguments_count: u32, arguments_stride: u32, bytes_data: *mut u8, bytes_capacity: u32, bytes_count: u32 });

impl Default for Participant {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            account_id: [0; 16],
            device_id: [0; 16],
            instance_id: [0; 16],
            build_id: [0; 32],
        }
    }
}

super::record::record!(Participant, 88, { account_id: [u8; 16], device_id: [u8; 16], instance_id: [u8; 16], build_id: [u8; 32] });
