#![no_main]
use libfuzzer_sys::fuzz_target;
// Coverage retains native SDK calls; link their production C ABI definitions.
use playbit as _;
use playbit_api::abi::syscall::Reader;

fuzz_target!(|input: &[u8]| {
    if let Ok(text) = playbit_api::abi::text::TextInput::decode(input) {
        let encoded = if let Some(edits) = text.edits {
            playbit_api::abi::text::TextInput::encode(text.text, &edits.collect::<Vec<_>>())
        } else {
            playbit_api::abi::text::TextInput::snapshot(text.text)
        }
        .expect("validated text event");
        assert_eq!(encoded, input);
    }
    if let Ok(page) = playbit_api::abi::bloblog::decode_page(input) {
        let mut encoded = vec![0; playbit_api::abi::bloblog::MAX_PAGE_BYTES];
        let length = playbit_api::abi::bloblog::encode_page(
            &mut encoded,
            page.entries,
            page.cursor,
            page.more,
        )
        .expect("validated bloblog page");
        assert_eq!(&encoded[..length], input);
    }
    if let Ok(page) = playbit_api::abi::kv::decode_page(input) {
        let mut encoded = vec![0; playbit_api::abi::kv::MAX_PAGE_BYTES];
        let length =
            playbit_api::abi::kv::encode_page(&mut encoded, page.root, page.keys, page.next)
                .expect("validated KV page");
        assert_eq!(&encoded[..length], input);
    }
    if let Ok(chunk) = playbit_api::abi::kv::decode_chunk(input) {
        let mut encoded = vec![0; playbit_api::abi::kv::MAX_CHUNK_BYTES];
        let length =
            playbit_api::abi::kv::encode_chunk(&mut encoded, chunk).expect("validated KV chunk");
        assert_eq!(&encoded[..length], input);
    }
    if let Ok(surface) = playbit_api::abi::environment::Surface::decode(input) {
        assert_eq!(surface.encode().expect("decoded surface"), input);
    }
    let split = input.len().min(32);
    let (operations, bytes) = input.split_at(split);
    let Ok(mut reader) = Reader::new(bytes) else { return };
    for operation in operations {
        let before = reader.position();
        match operation % 14 {
            0 => {
                let _ = reader.u8();
            }
            1 => {
                let _ = reader.u32();
            }
            2 => {
                let _ = reader.u64();
            }
            3 => {
                let _ = reader.id();
            }
            4 => {
                let _ = reader.bool();
            }
            5 => {
                let _ = reader.handle();
            }
            6 => {
                let _ = reader.operation_handle();
            }
            7 => {
                let _ = reader.bytes();
            }
            8 => {
                let _ = reader.string();
            }
            9 => {
                let _ = reader.count(u32::from(*operation));
            }
            10 => {
                let _ = reader.take(usize::from(*operation));
            }
            11 => {
                assert_eq!(reader.is_empty(), reader.remaining().is_empty());
            }
            12 => {
                let _ = reader.finish();
                reader = Reader::new(bytes).expect("same bounded input");
                continue;
            }
            _ => {
                assert_eq!(reader.position() + reader.remaining().len(), bytes.len());
            }
        }
        assert!(reader.position() >= before);
        assert!(reader.position() <= bytes.len());
    }
    let _ = reader.finish();
});
