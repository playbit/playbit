#![no_main]

use libfuzzer_sys::fuzz_target;
use playbit::engine::{
    ArchivedRequest, Request,
    ipc::codec::{self, Bytes},
    reply::{ArchivedReply, Reply},
};
use playbit::store::{Id, bloblog};
use playbit::ui::app_control;
use std::{
    io::{self, Read},
    sync::OnceLock,
};

// Valid archives come from the production serializer, so schema changes update
// these seeds automatically. Mutations still enter only the checked decoder.
fn seeds() -> &'static (Vec<Bytes>, Vec<Bytes>) {
    static SEEDS: OnceLock<(Vec<Bytes>, Vec<Bytes>)> = OnceLock::new();
    SEEDS.get_or_init(|| {
        let id = Id::from_bytes([1; 16]);
        let log = playbit::store::EntryId::from_bytes([1; 15]).expect("nonzero fixture ID");
        let cursor = bloblog::Cursor::parse(&format!(
            "1/{id}/{log}/{}/0/{id}",
            playbit::identifier::encode(&[0; 8])
        ))
        .expect("valid fixture cursor");
        let root = playbit::package::VolumeInitializers::from_json(include_bytes!(
            "../../../guests/todo/playbit-volumes.json"
        ))
        .expect("valid TODO role declarations")
        .for_role(playbit::package::AutomaticVolumeRole::Session);
        let requests = [
            Request::Status,
            Request::Syscall {
                context: playbit::engine::service_call::Context::Tool {
                    handle: id,
                    incarnation: id,
                },
                operation: 1,
                mutation: 1,
                call: Box::new(playbit::runtime::host::DataCall {
                    volume: *id.as_bytes(),
                    operation: playbit::runtime::host::DataOperation::BloblogAppend {
                        append_id: [1; 16],
                        log: *log.as_bytes(),
                        blobs: vec![vec![], vec![0; 1024]],
                    },
                }),
            },
            Request::Syscall {
                context: playbit::engine::service_call::Context::Tool {
                    handle: id,
                    incarnation: id,
                },
                operation: 2,
                mutation: 0,
                call: Box::new(playbit::runtime::host::DataCall {
                    volume: *id.as_bytes(),
                    operation: playbit::runtime::host::DataOperation::BloblogRead {
                        log: *log.as_bytes(),
                        cursor: Some(cursor.clone()),
                    },
                }),
            },
            Request::InitializeRoot {
                volume: id,
                name: "Todo".into(),
                declaration: Box::new(root),
            },
        ];
        let replies = [
            Reply::Protocol { protocol: codec::ABI, version: playbit::VERSION.into() },
            Reply::Error { error: "error\n雪".into() },
            Reply::Syscall(playbit::engine::service_call::Outcome {
                context: playbit::engine::service_call::Context::Tool {
                    handle: id,
                    incarnation: id,
                },
                volume: id,
                operation: 2,
                mutation: 0,
                kind: playbit::runtime::host::DataKind::BloblogRead as u32,
                revision: Some((id, 1)),
                status: 0,
                result: Some(playbit::runtime::host::DataResult::BloblogPage(bloblog::Page {
                    entries: vec![bloblog::Entry {
                        log,
                        id,
                        clock: 1,
                        nonce: id,
                        data: vec![0, 255, 1],
                    }],
                    cursor,
                    more: false,
                })),
            }),
        ];
        let (mut requests, mut replies): (Vec<Bytes>, Vec<Bytes>) = (
            requests
                .iter()
                .map(|value| {
                    let bytes = codec::encode(value, codec::MAX_ARCHIVE).unwrap();
                    codec::access::<ArchivedRequest>(&bytes).expect("valid request seed");
                    bytes
                })
                .collect(),
            replies
                .iter()
                .map(|value| {
                    let bytes = codec::encode(value, codec::MAX_ARCHIVE).unwrap();
                    codec::access::<ArchivedReply>(&bytes).expect("valid reply seed");
                    bytes
                })
                .collect(),
        );
        for operation in [
            app_control::Operation::Frame,
            app_control::Operation::Logs { after: None },
            app_control::Operation::Input { kind: 3, target: 100, data: vec![0, 255] },
            app_control::Operation::Quit,
        ] {
            requests.push(
                codec::encode(
                    &app_control::Request { instance: id, generation: 1, operation },
                    app_control::REQUEST_LIMIT,
                )
                .unwrap(),
            );
        }
        for reply in [
            app_control::Reply::Frame {
                instance: id,
                generation: 1,
                build: playbit::identifier::encode(&[0xab; 32]),
                scene: vec![0, 255],
            },
            app_control::Reply::Logs(app_control::LogBatch {
                cursor: app_control::LogCursor { instance: id, generation: 1, next: 1 },
                lines: vec![app_control::LogEntry {
                    timestamp_micros: Some(1),
                    severity: app_control::LogSeverity::Info,
                    message: "log".into(),
                }],
                dropped: 0,
                reset: false,
            }),
            app_control::Reply::Accepted { instance: id, generation: 1 },
        ] {
            replies.push(codec::encode(&reply, app_control::REPLY_LIMIT).unwrap());
        }
        (requests, replies)
    })
}

fn validate(bytes: &[u8]) {
    let _ = codec::access::<ArchivedRequest>(bytes);
    let _ = codec::access::<ArchivedReply>(bytes);
    let _ = codec::access::<app_control::ArchivedRequest>(bytes);
    let _ = codec::access::<app_control::ArchivedReply>(bytes);
    // Exercise alignment errors as well as root/relative-pointer bounds. No
    // unchecked access or materialization of unadmitted owned objects occurs.
    if !bytes.is_empty() {
        let mut shifted = Bytes::with_capacity(bytes.len() + 1);
        shifted.push(0);
        shifted.extend_from_slice(bytes);
        let _ = codec::access::<ArchivedRequest>(&shifted[1..]);
        let _ = codec::access::<ArchivedReply>(&shifted[1..]);
        let _ = codec::access::<app_control::ArchivedRequest>(&shifted[1..]);
        let _ = codec::access::<app_control::ArchivedReply>(&shifted[1..]);
    }
}

struct Fragmented<'a> {
    remaining: &'a [u8],
    quantum: usize,
}
impl Read for Fragmented<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = output.len().min(self.remaining.len()).min(self.quantum);
        output[..count].copy_from_slice(&self.remaining[..count]);
        self.remaining = &self.remaining[count..];
        Ok(count)
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 || data.len() > codec::MAX_ARCHIVE {
        return;
    }
    let mode = data[0] % 6;
    let offset = u16::from_le_bytes([data[2], data[3]]) as usize;
    let payload = &data[4..];
    let mut bytes = Bytes::new();
    if mode == 0 || mode == 4 {
        bytes.extend_from_slice(payload);
    } else {
        let (requests, replies) = seeds();
        let list = if mode == 2 { replies } else { requests };
        bytes.extend_from_slice(&list[data[1] as usize % list.len()]);
        if mode == 3 {
            bytes.resize(offset % (bytes.len() + 1), 0);
        } else if mode == 5 {
            let mut frame = Vec::new();
            codec::write(&mut frame, &bytes).unwrap();
            bytes.clear();
            bytes.extend_from_slice(&frame);
        }
        let start = offset % (bytes.len() + 1);
        let count = payload.len().min(bytes.len() - start);
        bytes[start..start + count].copy_from_slice(&payload[..count]);
    }
    if mode == 4 || mode == 5 {
        // Credit may reject a syntactically valid frame before allocation.
        let credit = if data[1] & 128 == 0 { codec::MAX_ARCHIVE } else { offset };
        let mut reader = Fragmented { remaining: &bytes, quantum: 1 + data[1] as usize % 64 };
        if let Ok(archive) = codec::read(&mut reader, credit) {
            assert!(archive.capacity() <= credit);
            let consumed = bytes.len() - reader.remaining.len();
            assert_eq!(consumed, 4 + codec::PREFIX_LEN + archive.len());
            let mut canonical = Vec::new();
            codec::write(&mut canonical, &archive).unwrap();
            // Rewriting a frame creates new diagnostic tokens. Only identity,
            // length and archive bytes are canonical; incoming tokens carry no
            // decoder authority and are intentionally not re-exported.
            assert_eq!(&canonical[..4 + codec::IDENTITY_LEN], &bytes[..4 + codec::IDENTITY_LEN]);
            assert_eq!(&canonical[codec::HEADER_LEN..], &bytes[codec::HEADER_LEN..consumed]);
            validate(&archive);
        }
    } else {
        validate(&bytes);
    }
});
