//! Canonical opaque identifier text, shared with local trace storage.
pub use playbit_trace::identifier::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_shell_vectors_cover_every_supported_width() {
        // Native, browser, Swift and developer tools consume these exact spellings.
        let vectors: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/identifiers.json"))
                .expect("shared vectors");
        for vector in vectors {
            let bytes: Vec<u8> = serde_json::from_value(vector["bytes"].clone()).expect("bytes");
            let text = vector["text"].as_str().expect("text");
            assert_eq!(encode(&bytes), text);
            assert!(valid(text, bytes.len()));
        }
    }

    #[test]
    fn canonical_vectors_and_all_leading_zero_widths() {
        // Check known Bitcoin-alphabet vectors and every leading-zero count.
        assert_eq!(encode(&[0; 16]), "1111111111111111");
        assert_eq!(encode(&[255; 16]), "YcVfxkQb6JRzqk5kF2tNLv");
        assert_eq!(encode(&[255; 32]), "JEKNVnkbo3jma5nREBBJCDoXFVeKkD56V3xKrvRmWxFG");
        fn leading_zeros<const N: usize>() {
            for zeros in 0..=N {
                for byte in 1..=255 {
                    let mut bytes = [byte; N];
                    bytes[..zeros].fill(0);
                    assert_eq!(decode::<N>(&encode(&bytes)), Ok(bytes));
                }
            }
        }
        leading_zeros::<15>();
        leading_zeros::<16>();
        leading_zeros::<32>();
        for invalid in [
            "",
            "0",
            "O",
            "I",
            "l",
            "é",
            "🦀",
            "111111111111111",
            "11111111111111111",
            "zzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert!(decode::<16>(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn powers_and_adjacent_values_have_exact_digit_spellings() {
        fn check<const N: usize>() {
            let mut power = [0u8; N];
            power[N - 1] = 1;
            // Independent byte arithmetic exercises every group boundary,
            // including partial groups, leading zero bytes and carries.
            for exponent in 1..MAX_TEXT {
                let mut carry = 0u16;
                for byte in power.iter_mut().rev() {
                    carry += u16::from(*byte) * 58;
                    *byte = carry as u8;
                    carry >>= 8;
                }
                if carry != 0 {
                    break;
                }
                let mut below = power;
                for byte in below.iter_mut().rev() {
                    let (next, borrow) = byte.overflowing_sub(1);
                    *byte = next;
                    if !borrow {
                        break;
                    }
                }
                let mut above = power;
                for byte in above.iter_mut().rev() {
                    let (next, overflow) = byte.overflowing_add(1);
                    *byte = next;
                    if !overflow {
                        break;
                    }
                }
                for (bytes, digits) in [
                    (below, "z".repeat(exponent)),
                    (power, format!("2{}", "1".repeat(exponent))),
                    (above, format!("2{}2", "1".repeat(exponent - 1))),
                ] {
                    let zeros = bytes.iter().take_while(|&&byte| byte == 0).count();
                    let text = format!("{}{digits}", "1".repeat(zeros));
                    assert_eq!(encode(&bytes), text);
                    assert_eq!(decode::<N>(&text), Ok(bytes));
                }
            }
        }
        check::<15>();
        check::<16>();
        check::<32>();
    }
}

/// JSON adapters for ABI structs whose in-memory identity is a byte array.
/// Binary/rkyv layouts do not use these adapters.
pub mod serde {
    use ::serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &[u8; N],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&super::Text(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<[u8; N], D::Error> {
        super::decode(&String::deserialize(deserializer)?).map_err(::serde::de::Error::custom)
    }
}
pub mod optional {
    use ::serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &Option<[u8; N]>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        bytes.as_ref().map(|bytes| super::encode(bytes)).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<Option<[u8; N]>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| super::decode(&text).map_err(::serde::de::Error::custom))
            .transpose()
    }
}
pub mod list {
    use ::serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &[[u8; N]],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        bytes.iter().map(|bytes| super::encode(bytes)).collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<Vec<[u8; N]>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .iter()
            .map(|text| super::decode(text).map_err(::serde::de::Error::custom))
            .collect()
    }
}
pub mod optional_list {
    use ::serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &Option<Vec<[u8; N]>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        bytes
            .as_ref()
            .map(|bytes| bytes.iter().map(|bytes| super::encode(bytes)).collect::<Vec<_>>())
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<Option<Vec<[u8; N]>>, D::Error> {
        Option::<Vec<String>>::deserialize(deserializer)?
            .map(|values| {
                values
                    .iter()
                    .map(|text| super::decode(text).map_err(::serde::de::Error::custom))
                    .collect()
            })
            .transpose()
    }
}
