use playbit::engine::ffi::{Change, Changes, Volume};
fn main() {
    println!(
        "{},{},{},{},{},{},{},{},{},{},{}",
        size_of::<Volume>(),
        align_of::<Volume>(),
        std::mem::offset_of!(Volume, name),
        std::mem::offset_of!(Volume, name_length),
        size_of::<Change>(),
        align_of::<Change>(),
        std::mem::offset_of!(Change, sequence),
        size_of::<Changes>(),
        align_of::<Changes>(),
        std::mem::offset_of!(Changes, gap_generation),
        std::mem::offset_of!(Changes, remaining)
    );
}
