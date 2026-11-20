//! Natural-language display names use UCA, with ICU's locale-neutral root data.
//! This order is for presentation, including cut-fenced native browser pages;
//! persistent keys and storage cursors retain their existing identity order.
use icu_collator::{Collator, CollatorBorrowed, options::CollatorOptions};
use std::{cmp::Ordering, sync::LazyLock};

static COLLATOR: LazyLock<CollatorBorrowed<'static>> = LazyLock::new(|| {
    Collator::try_new(Default::default(), CollatorOptions::default())
        .expect("bundled root collation data")
});

/// Canonically equivalent names compare equal; callers break ties by identity.
pub fn compare(a: &str, b: &str) -> Ordering {
    COLLATOR.compare(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uca_orders_letters_before_accents_and_case() {
        for (a, b) in [
            ("Archive", "Archive UI"),
            ("Archive UI", "Family"),
            ("apple", "Banana"),
            ("e", "é"),
            ("éclair", "Family"),
            ("б", "Я"),
        ] {
            assert_eq!(compare(a, b), Ordering::Less, "{a:?} before {b:?}");
            assert_eq!(compare(b, a), Ordering::Greater);
        }
        assert_ne!(compare("a", "A"), Ordering::Equal, "tertiary case distinction");
    }

    #[test]
    fn canonical_equivalents_compare_equal_without_changing_text() {
        for (a, b) in [("Café", "Cafe\u{301}"), ("가", "\u{1100}\u{1161}")] {
            assert_eq!(compare(a, b), Ordering::Equal);
        }
    }
}
