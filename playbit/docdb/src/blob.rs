use std::fs;
// use std::io::{Read, Write};
// use std::net::TcpStream;

// TODO Encryption

pub fn put(name: &str, old: &[u8], new: &[u8]) -> bool {
    let lock_path = format!("data/{}.lock", name);
    let main_path = format!("data/{}", name);

    match fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
        Ok(file) => {
            let contents = fs::read(&main_path).unwrap_or(Vec::new());
            let m = contents == old;
            if m {
                fs::write(&main_path, new).unwrap();
            }
            #[cfg_attr(
                all(target_family = "wasm", target_os = "unknown"),
                allow(
                    clippy::drop_non_drop,
                    reason = "Wasm File has no destructor; native locks must close before removal."
                )
            )]
            drop(file);
            fs::remove_file(&lock_path).unwrap();
            m
        }
        Err(_error) => false,
    }
}

pub fn get(name: &str) -> Option<Vec<u8>> {
    let lock_path = format!("data/{}.lock", name);
    let main_path = format!("data/{}", name);

    match fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
        Ok(file) => {
            let contents = fs::read(&main_path).unwrap_or(Vec::new());
            #[cfg_attr(
                all(target_family = "wasm", target_os = "unknown"),
                allow(
                    clippy::drop_non_drop,
                    reason = "Wasm File has no destructor; native locks must close before removal."
                )
            )]
            drop(file);
            fs::remove_file(&lock_path).unwrap();
            Some(contents)
        }
        Err(_error) => None,
    }
}

#[cfg(test)]
pub fn reset() {
    // For testing only
    let _ = fs::remove_dir_all("data");
    fs::create_dir("data").unwrap();
    fs::write("data/HEAD", [0u8; 16]).unwrap();
}

/*
pub fn put(name: &str, old: &[u8], new: &[u8]) -> bool {
    let header: [u8; 4] = [1u8, name.len() as u8, old.len() as u8, new.len() as u8];
    let mut stream = TcpStream::connect("18.175.168.31:80").unwrap();
    stream.write(&header).unwrap();
    stream.write(name.as_bytes()).unwrap();
    stream.write(old).unwrap();
    stream.write(new).unwrap();
    todo!(); // TODO Return if old matched and put was successful
}

pub fn get(name: &str) -> Option<Vec<u8>> {
    let header: [u8; 4] = [0u8, name.len() as u8, 0u8, 0u8];
    let mut stream = TcpStream::connect("18.175.168.31:80").unwrap();
    stream.write(&header).unwrap();
    stream.write(name.as_bytes()).unwrap();
    let mut len = [0u8];
    stream.read(&mut len).unwrap();
    let mut buf = [0u8].repeat(len[0] as usize);
    stream.read(&mut buf).unwrap();
    Some(buf)
}

pub fn reset() {
    todo!();
}
*/
