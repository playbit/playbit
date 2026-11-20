//! Bounded local affordance checks; AP remains the authentication authority.
pub fn email(value: &str) -> bool {
    if value.len() > 254 {
        return false;
    }
    let value = value.trim();
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        })
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Called only for an invalid key; match the parser's whitespace handling.
pub fn account_key_error(value: &str) -> &'static str {
    match value.trim().len().cmp(&crate::store::account_key::TEXT_BYTES) {
        std::cmp::Ordering::Less => "Account key too short",
        std::cmp::Ordering::Greater => "Account key too long",
        std::cmp::Ordering::Equal => "Invalid account key",
    }
}

pub fn code(value: &str) -> bool {
    crate::network::ap::registration::valid_email_code(value.as_bytes())
}

pub fn display_name(value: &str) -> bool {
    value.len() <= 256 && crate::store::account::validate_display_name(value.trim()).is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[test]
    fn key_length_feedback_matches_trimmed_parser_input() {
        assert_eq!(account_key_error("a123"), "Account key too short");
        assert_eq!(account_key_error(&"a".repeat(24)), "Account key too long");
        assert_eq!(account_key_error(&"0".repeat(23)), "Invalid account key");
        assert_eq!(account_key_error("   a123   "), "Account key too short");
        assert_eq!(account_key_error(&format!("  {}  ", "0".repeat(23))), "Invalid account key");
    }

    #[test]
    fn fields_require_a_complete_address_or_six_ascii_digits() {
        for value in [
            "",
            "alice",
            "alice@",
            "a@b",
            "a@b.",
            "a@.com",
            "a@example..com",
            "a@-example.com",
            "a@example-.com",
            "a@example_com",
            "a@example.c/m",
            "@example.invalid",
            "a@@example.invalid",
            "a b@example.invalid",
            "a\0@example.invalid",
            &"a".repeat(255),
        ] {
            assert!(!email(value), "{value:?}");
        }
        for value in [
            "alice@example.invalid",
            " Alice+todo@example.invalid ",
            "用户@example.invalid",
            "a@sub.example.com",
            "a@xn--bcher-kva.example",
            "用户@例子.测试",
        ] {
            assert!(email(value), "{value:?}");
        }
        for value in ["", "12345", "1234567", "12345678", "123 456", "１２３４５６"] {
            assert!(!code(value));
        }
        assert!(code("000012"));
    }

    #[test]
    fn display_names_validate_bounded_unicode_and_trim_only_at_submission() {
        for value in ["Robin", " Robin 雨 🌱 ", &"é".repeat(128)] {
            assert!(display_name(value), "{value:?}");
            assert!(crate::store::account::validate_display_name(value.trim()).is_ok());
        }
        for value in ["", "   ", "Ro\nbin", "Robin\0", &"é".repeat(129), &"x".repeat(257)] {
            assert!(!display_name(value), "{value:?}");
        }
    }

    #[test]
    #[ignore = "optimized local form-processing diagnostic; excludes UI rendering"]
    #[allow(
        clippy::assertions_on_constants,
        reason = "The ignored timing diagnostic requires an optimized build."
    )]
    fn benchmark_account_field_validation() {
        assert!(!cfg!(debug_assertions));
        let address = format!("{}@example.invalid", "a".repeat(238));
        let name = "é".repeat(128);
        assert_eq!(address.len(), 254);
        let mut samples = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            let start = std::time::Instant::now();
            assert!(email(std::hint::black_box(&address)));
            assert!(code(std::hint::black_box("012345")));
            assert!(display_name(std::hint::black_box(&name)));
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        crate::diagnostic!(
            "account email+code+name validation n={} p99={} ns max={} ns",
            samples.len(),
            samples[9899],
            samples[9999]
        );
        assert!(samples[9899] < 100_000);
    }
}
