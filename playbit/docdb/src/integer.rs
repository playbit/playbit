pub trait Integer: Clone + Copy + std::fmt::Debug {
    fn generic_to_le_bytes(self, buffer: &mut [u8; 8]) -> &[u8];
    fn generic_from_le_bytes(bytes: &[u8]) -> Self;
    fn generic_from_le_bytes_or_zero(bytes: &[u8]) -> Self;
}

macro_rules! impl_integer {
    ($T:ty) => {
        impl Integer for $T {
            fn generic_to_le_bytes(self, buffer: &mut [u8; 8]) -> &[u8] {
                buffer[0..size_of::<Self>()].copy_from_slice(&self.to_le_bytes());
                &buffer[0..size_of::<Self>()]
            }

            fn generic_from_le_bytes(bytes: &[u8]) -> Self {
                Self::from_le_bytes(bytes.try_into().unwrap())
            }

            fn generic_from_le_bytes_or_zero(bytes: &[u8]) -> Self {
                let Ok(bytes) = bytes.try_into() else {
                    return 0;
                };
                Self::from_le_bytes(bytes)
            }
        }
    };
}

impl_integer!(i8);
impl_integer!(i16);
impl_integer!(i32);
impl_integer!(i64);

impl_integer!(u8);
impl_integer!(u16);
impl_integer!(u32);
impl_integer!(u64);
