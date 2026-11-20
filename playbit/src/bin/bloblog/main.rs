use playbit::client::{Id, bloblog};
use playbit::engine::{Request, Result, ipc, profile_path, reply::Reply};
use playbit::store::EntryId;
use std::io::Read;

const HELP: &str = "bloblog — opaque multiplayer log playground

  bloblog volumes
  bloblog create-volume NAME
  bloblog create VOLUME NAME
  bloblog put VOLUME LOG APPEND_ID TEXT [TEXT ...]
  bloblog put-file VOLUME LOG APPEND_ID FILE [FILE ...]
  bloblog get VOLUME LOG [CURSOR]
  bloblog watch VOLUME LOG [CURSOR]
  bloblog sync

VOLUME is a base58-encoded 16-byte ID; LOG is a base58-encoded 15-byte entry ID.
Use - as a FILE to read stdin. Results are JSON; blob data is an array of bytes.
get returns one bounded page; follow cursor while more=true. watch waits for
service change notifications (no polling). put waits for remote durability.
APPEND_ID is a caller-provided base58 16-byte ID; reuse it with the same payload
to resolve an uncertain result. PLAYBIT_PROFILE selects a separate local engine profile.
";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        playbit::diagnostic!("bloblog: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        print!("{HELP}");
        return Ok(());
    };
    if matches!(command, "-h" | "--help" | "help") {
        print!("{HELP}");
        return Ok(());
    }
    playbit::engine::profile::initialize()?;
    let profile = profile_path();
    playbit::platform::failure::startup(&profile);
    playbit::platform::failure::install_native(&profile);
    match command {
        "volumes" if args.len() == 1 => {
            let Reply::Status(status) =
                ipc::call_archive(&profile, &Request::Status)?.into_owned()?
            else {
                return Err("unexpected status reply".into());
            };
            println!("{}", serde_json::to_string(&status.volumes)?);
        }
        "create-volume" if args.len() == 2 => {
            let reply = ipc::call_archive(
                &profile,
                &Request::CreateVolume { share_key_with_service: true, name: args[1].clone() },
            )?
            .into_owned()?;
            println!("{}", serde_json::to_string(&reply)?);
        }
        "create" if args.len() == 3 => {
            let entry = bloblog::create(Id::parse(&args[1])?, args[2].clone()).await?;
            println!("{}", serde_json::to_string(&entry)?);
        }
        "sync" if args.len() == 1 => {
            let reply = ipc::call_archive(&profile, &Request::Sync)?.into_owned()?;
            println!("{}", serde_json::to_string(&reply)?);
        }
        "put" | "put-file" if args.len() >= 5 => append(command, &args).await?,
        "get" | "watch" if (3..=4).contains(&args.len()) => read(command, &args, &profile).await?,
        _ => return Err(format!("invalid arguments\n\n{HELP}").into()),
    }
    Ok(())
}

async fn append(command: &str, args: &[String]) -> Result<()> {
    if args.len() - 4 > bloblog::MAX_BLOBS {
        return Err("too many blobs".into());
    }
    let volume = Id::parse(&args[1])?;
    let log = EntryId::parse(&args[2])?;
    let mut blobs = Vec::new();
    let append_id = Id::parse(&args[3])?;
    for arg in &args[4..] {
        let data = if command == "put" {
            arg.as_bytes().to_vec()
        } else {
            let reader: Box<dyn Read> = if arg == "-" {
                Box::new(std::io::stdin())
            } else {
                Box::new(std::fs::File::open(arg)?)
            };
            let mut bytes = Vec::new();
            reader.take(bloblog::MAX_BLOB_BYTES as u64 + 1).read_to_end(&mut bytes)?;
            bytes
        };
        if data.len() > bloblog::MAX_BLOB_BYTES {
            return Err("blob exceeds 32 KiB".into());
        }
        blobs.push(data);
    }
    println!(
        "{}",
        serde_json::json!({"cursor":bloblog::put(volume, log, append_id, blobs).await?,"durability":"remote"})
    );
    Ok(())
}

async fn read(command: &str, args: &[String], profile: &std::path::Path) -> Result<()> {
    let volume = Id::parse(&args[1])?;
    let log = EntryId::parse(&args[2])?;
    let mut cursor = args.get(3).map(|s| bloblog::Cursor::parse(s)).transpose()?;
    // Capture the change boundary BEFORE reading, including on first
    // watch. An append racing a read makes the next wait return at once.
    let mut after = None;
    loop {
        let wait =
            if command == "watch" { Some(ipc::ChangeWait::connect(profile, after)?) } else { None };
        loop {
            let page = bloblog::get(volume, log, cursor).await?;
            cursor = Some(page.cursor.clone());
            if command == "get" || !page.entries.is_empty() {
                println!("{}", serde_json::to_string(&page)?);
            }
            if command == "get" {
                return Ok(());
            }
            if !page.more {
                break;
            }
        }
        let reply = tokio::task::spawn_blocking(move || {
            wait.expect("the wait mode installed a subscription").receive()
        })
        .await??;
        after = Some(reply.token);
    }
}
