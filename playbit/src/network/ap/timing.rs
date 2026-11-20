//! Bounded, operation-private AP duration annotations. They carry no clock
//! position, resource identity, account identity, count or payload information.
pub mod auth;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor},
    ser::SerializeSeq,
};
use std::fmt;

pub const MAX_STAGES: usize = 8;
pub const MAX_STAGE_NAME_BYTES: usize = 64;
pub const MAX_ENCODED_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Stage {
    #[serde(rename = "ap.admission_check")]
    AdmissionCheck,
    #[serde(rename = "ap.authority_check")]
    AuthorityCheck,
    #[serde(rename = "ap.durable_merge")]
    DurableMerge,
    #[serde(rename = "ap.storage_sync")]
    StorageSync,
    #[serde(rename = "ap.mail_send")]
    MailSend,
    #[serde(rename = "ap.storage_wait")]
    StorageWait,
    #[serde(rename = "ap.request_read")]
    RequestRead,
    #[serde(rename = "ap.operation")]
    Operation,
}
impl Stage {
    pub const ALL: [Self; MAX_STAGES] = [
        Self::AdmissionCheck,
        Self::AuthorityCheck,
        Self::DurableMerge,
        Self::StorageSync,
        Self::MailSend,
        Self::StorageWait,
        Self::RequestRead,
        Self::Operation,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::AdmissionCheck => "ap.admission_check",
            Self::AuthorityCheck => "ap.authority_check",
            Self::DurableMerge => "ap.durable_merge",
            Self::StorageSync => "ap.storage_sync",
            Self::MailSend => "ap.mail_send",
            Self::StorageWait => "ap.storage_wait",
            Self::RequestRead => "ap.request_read",
            Self::Operation => "ap.operation",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    pub stage: Stage,
    // Canonical decimal avoids loss of u64 precision in browser JSON readers.
    #[serde(with = "nanoseconds")]
    pub duration_ns: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timings(Vec<Timing>);
impl Timings {
    pub fn entries(&self) -> &[Timing] {
        &self.0
    }
    pub fn push(&mut self, timing: Timing) -> Result<(), &'static str> {
        if !stage_admissible(&self.0, timing.stage) {
            return Err("duplicate or excessive AP timing stage");
        }
        if self.0.capacity() < MAX_STAGES {
            self.0.reserve_exact(MAX_STAGES - self.0.len());
        }
        self.0.push(timing);
        Ok(())
    }
}
// Kept independent of allocation so bounded admission is verified over every
// subset. Vec growth and rejected-operation mutation have direct regressions.
fn stage_admissible(entries: &[Timing], stage: Stage) -> bool {
    entries.len() < MAX_STAGES && !entries.iter().any(|old| old.stage == stage)
}
impl Serialize for Timings {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for entry in &self.0 {
            seq.serialize_element(entry)?;
        }
        seq.end()
    }
}
impl<'de> Deserialize<'de> for Timings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Bounded;
        impl<'de> Visitor<'de> for Bounded {
            type Value = Timings;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("at most eight distinct AP duration stages")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Timings, A::Error> {
                let mut value = Timings::default();
                let mut names = Vec::<StageName>::with_capacity(MAX_STAGES);
                let mut encoded_bytes = b",\"timing\":[]".len();
                while let Some(entry) = seq.next_element::<WireTiming>()? {
                    if names.len() == MAX_STAGES || names.iter().any(|old| old == &entry.stage) {
                        return Err(de::Error::custom("duplicate or excessive AP timing stage"));
                    }
                    let digits = if entry.duration_ns == 0 {
                        1
                    } else {
                        entry.duration_ns.ilog10() as usize + 1
                    };
                    encoded_bytes += b"{\"stage\":\"\",\"duration_ns\":\"\"}".len()
                        + entry.stage.0.len()
                        + digits
                        + usize::from(!names.is_empty());
                    if encoded_bytes > MAX_ENCODED_BYTES {
                        return Err(de::Error::custom("AP timing encoding exceeds budget"));
                    }
                    if let Some(stage) =
                        Stage::ALL.into_iter().find(|stage| stage.name() == entry.stage.0)
                    {
                        value
                            .push(Timing { stage, duration_ns: entry.duration_ns })
                            .map_err(de::Error::custom)?;
                    }
                    names.push(entry.stage);
                }
                Ok(value)
            }
        }
        deserializer.deserialize_seq(Bounded)
    }
}

// This is a client instrumentation projection, never a relay/rewriter. Unknown
// names are validated and charged before being omitted from the static recorder.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTiming {
    stage: StageName,
    #[serde(with = "nanoseconds")]
    duration_ns: u64,
}
#[derive(PartialEq, Eq)]
struct StageName(String);
impl<'de> Deserialize<'de> for StageName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Name;
        impl Visitor<'_> for Name {
            type Value = StageName;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an AP stage name of at most 64 ASCII bytes")
            }
            fn visit_str<E: de::Error>(self, name: &str) -> Result<StageName, E> {
                if name.len() > MAX_STAGE_NAME_BYTES
                    || !name.starts_with("ap.")
                    || name.len() == 3
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'.' | b'_')
                    })
                {
                    return Err(E::custom("invalid AP timing stage name"));
                }
                Ok(StageName(name.into()))
            }
        }
        deserializer.deserialize_str(Name)
    }
}

mod nanoseconds {
    use super::*;
    pub fn serialize<S: Serializer>(ns: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(ns)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct Decimal;
        impl Visitor<'_> for Decimal {
            type Value = u64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a canonical unsigned nanosecond decimal")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<u64, E> {
                if text.is_empty()
                    || text.len() > 20
                    || !text.bytes().all(|b| b.is_ascii_digit())
                    || (text.len() > 1 && text.starts_with('0'))
                {
                    return Err(E::custom("invalid AP timing duration"));
                }
                text.parse().map_err(|_| E::custom("AP timing duration overflow"))
            }
        }
        deserializer.deserialize_str(Decimal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maximum_reply_annotation_fits_budget_without_browser_precision_loss() {
        let mut timings = Timings::default();
        for stage in Stage::ALL {
            timings.push(Timing { stage, duration_ns: u64::MAX }).expect("distinct bounded stage");
        }
        let encoded = serde_json::to_vec(&timings).expect("bounded wire value");
        let field_bytes = encoded.len() + b",\"timing\":".len();
        assert!(field_bytes <= MAX_ENCODED_BYTES);
        assert_eq!(
            serde_json::from_slice::<Timings>(&encoded).expect("lossless duration"),
            timings
        );
        assert!(timings.push(Timing { stage: Stage::Operation, duration_ns: 0 }).is_err());
        crate::diagnostic!("maximum AP timing JSON bytes including field: {field_bytes}");
    }
    #[test]
    fn rejected_insertions_preserve_every_subset_and_accepted_entries() {
        let stages = Stage::ALL;
        for selected in 0..=u8::MAX {
            let mut original = Timings::default();
            for stage in stages {
                if selected & (1 << stage as u8) != 0 {
                    original
                        .push(Timing { stage, duration_ns: u64::MAX })
                        .expect("distinct subset stage");
                }
            }
            for stage in stages {
                let mut copy = original.clone();
                let entry = Timing { stage, duration_ns: 7 };
                if copy.push(entry).is_err() {
                    assert_eq!(copy, original);
                } else {
                    assert_eq!(&copy.entries()[..original.entries().len()], original.entries());
                    assert_eq!(copy.entries().last(), Some(&entry));
                }
                assert!(copy.entries().len() <= MAX_STAGES);
                assert!(copy.0.capacity() <= MAX_STAGES);
            }
        }
    }
    #[test]
    fn extending_a_clone_keeps_the_fixed_allocation_allowance() {
        let mut original = Timings::default();
        for stage in [
            Stage::AdmissionCheck,
            Stage::AuthorityCheck,
            Stage::DurableMerge,
            Stage::StorageSync,
            Stage::StorageWait,
        ] {
            original.push(Timing { stage, duration_ns: 1 }).expect("distinct fixture stage");
        }
        let mut copy = original.clone();
        copy.push(Timing { stage: Stage::Operation, duration_ns: u64::MAX })
            .expect("unused stage in clone");
        assert!(copy.0.capacity() <= MAX_STAGES);
        assert_eq!(original.entries().len(), 5);
        assert_eq!(copy.entries().len(), 6);
    }
    #[test]
    fn unknown_stages_are_bounded_and_do_not_create_dynamic_trace_names() {
        let parsed: Timings = serde_json::from_str(
            r#"[
            {"stage":"ap.future_storage","duration_ns":"18446744073709551615"},
            {"stage":"ap.operation","duration_ns":"7"}
        ]"#,
        )
        .expect("optional future duration");
        assert_eq!(parsed.entries(), &[Timing { stage: Stage::Operation, duration_ns: 7 }]);
        let name = format!("ap.{}", "x".repeat(MAX_STAGE_NAME_BYTES - 3));
        let entries: Vec<_> = (0..MAX_STAGES).map(|index| serde_json::json!({
            "stage": format!("{}{}", &name[..name.len()-1], index), "duration_ns": u64::MAX.to_string()
        })).collect();
        let encoded = serde_json::to_string(&entries).expect("bounded fixture");
        assert!(encoded.len() + b",\"timing\":".len() <= MAX_ENCODED_BYTES);
        assert!(
            serde_json::from_str::<Timings>(&encoded)
                .expect("eight unknown stages")
                .entries()
                .is_empty()
        );
        for invalid in [
            format!(r#"[{{"stage":"{}x","duration_ns":"1"}}]"#, name),
            r#"[{"stage":"ap.future","duration_ns":"1"},{"stage":"ap.future","duration_ns":"2"}]"#
                .into(),
            serde_json::to_string(&(0..=MAX_STAGES).map(|index|
                serde_json::json!({"stage":format!("ap.future{index}"),"duration_ns":"1"})
            ).collect::<Vec<_>>()).expect("excessive fixture"),
            r#"[{"stage":"ap.futüré","duration_ns":"1"}]"#.into(),
            r#"[{"stage":"ap.future\n","duration_ns":"1"}]"#.into(),
            r#"[{"stage":"ap.future","duration_ns":"1\n"}]"#.into(),
            r#"[{"stage":"ap.future","duration_ns":"01"}]"#.into(),
        ] {
            assert!(serde_json::from_str::<Timings>(&invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn malformed_or_private_timing_fields_are_rejected() {
        for raw in [
            r#"[{"stage":"ap.operation","duration_ns":1}]"#,
            r#"[{"stage":"ap.operation","duration_ns":"01"}]"#,
            r#"[{"stage":"ap.operation","duration_ns":"18446744073709551616"}]"#,
            r#"[{"stage":"ap.operation","duration_ns":"-1"}]"#,
            r#"[{"stage":"account.private","duration_ns":"1"}]"#,
            r#"[{"stage":"ap.operation","duration_ns":"1","account":"private"}]"#,
            r#"[{"stage":"ap.operation","duration_ns":"1"},{"stage":"ap.operation","duration_ns":"2"}]"#,
        ] {
            assert!(serde_json::from_str::<Timings>(raw).is_err(), "{raw}");
        }
        assert_eq!(
            serde_json::from_str::<Timings>("[]").expect("unavailable stages").entries(),
            []
        );
    }
}
