//! One bounded public-build transfer to a private staging file. Network reads
//! and filesystem work use the existing runtime; complete validation precedes
//! durable publication. Cancellation removes the unpublished candidate.
use super::{Result, cancellation::Canceled, remote::CanceledOperation};
use crate::{
    network::ap::{ApConnection, operation::ServiceRequest, service_client as wire},
    package::Package,
    store::Id,
};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
const MAX_DOWNLOAD: u64 = crate::package::MAX_BUNDLE_BYTES as u64;
struct Staging(PathBuf);
pub(crate) struct Downloaded {
    pub name: String,
    pub admission: Option<Vec<u8>>,
}
impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            crate::diagnostic!("Cannot remove incomplete build {}: {error}", self.0.display());
        }
    }
}
pub(super) async fn download(
    connection: &ApConnection,
    build: &str,
    path: &Path,
    canceled: Option<&Canceled>,
) -> Result<Downloaded> {
    if !crate::identifier::valid(build, 32) {
        return Err("invalid build identity".into());
    }
    let parent = path.parent().ok_or("build path has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let candidate = parent.join(format!(".download-{}", Id::generate()?));
    // File creation uses a blocking task internally. Await it before any
    // cancellable wait so cleanup cannot run before a detached create_new.
    let mut file =
        tokio::fs::OpenOptions::new().write(true).create_new(true).open(&candidate).await?;
    let staging = Staging(candidate);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    let request =
        ServiceRequest::GetBlob { operation_id: Id::generate()?.to_string(), hash: build.into() };
    let cancel = async {
        match canceled {
            Some(canceled) => canceled.raising().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(cancel);
    let mut stream = tokio::select! {
        biased;
        () = &mut cancel => return Err(CanceledOperation.into()),
        stream = tokio::time::timeout_at(deadline, wire::begin(connection, &request)) => stream??,
    };
    let received = tokio::select! {
        biased;
        () = &mut cancel => Err(CanceledOperation.into()),
        result = tokio::time::timeout_at(
            deadline, transfer(&mut stream, &request, &mut file),
        ) => match result {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        },
    };
    if received.is_err() {
        stream.cancel();
    }
    let admission = received?;
    drop(file);
    // Do not drop the staging owner while its blocking validation/fsync/rename
    // can still run. Cancellation may finish this immutable cache write, but
    // cannot turn it into a prepared/adopted session.
    let (file, build, path) = (staging.0.clone(), build.to_owned(), path.to_owned());
    let name = tokio::task::spawn_blocking(move || publish(&file, &build, &path)).await??;
    if canceled.is_some_and(Canceled::raised) {
        return Err(CanceledOperation.into());
    }
    Ok(Downloaded { name, admission })
}
async fn transfer(
    stream: &mut crate::network::ap::ApStream,
    request: &ServiceRequest,
    file: &mut tokio::fs::File,
) -> Result<Option<Vec<u8>>> {
    stream.finish()?;
    let header =
        wire::response(stream, request.operation_id()).await?.data.ok_or("missing build header")?;
    let length = header["content_length"].as_u64().ok_or("missing build length")?;
    let admission: Option<Vec<u8>> = serde_json::from_value(header["build_admission"].clone())?;
    if admission
        .as_ref()
        .is_some_and(|bytes| bytes.len() != crate::network::ap::publisher::BUILD_ADMISSION_BYTES)
    {
        return Err("invalid build admission receipt length".into());
    }
    if length > MAX_DOWNLOAD {
        return Err("build exceeds download limit".into());
    }
    let mut received = 0u64;
    while received < length {
        let chunk = crate::network::ap::service::data(stream.answer_with_body_limit(65536).await?)?;
        if chunk.is_empty() || chunk.len() > 65536 || chunk.len() as u64 > length - received {
            return Err("invalid build download length".into());
        }
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
    }
    wire::response(stream, request.operation_id()).await?;
    file.flush().await?;
    Ok(admission)
}
fn publish(staging: &Path, build: &str, path: &Path) -> Result<String> {
    let package = Package::open(staging)?;
    if package.id()? != build {
        return Err("build content hash mismatch".into());
    }
    let file = std::fs::File::open(staging)?;
    rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(std::io::Error::from)?;
    std::fs::rename(staging, path)?;
    let directory = std::fs::File::open(path.parent().ok_or("build path has no parent")?)?;
    rustix::io::retry_on_intr(|| rustix::fs::fsync(&directory)).map_err(std::io::Error::from)?;
    Ok(package.name().to_owned())
}
#[cfg(test)]
pub(super) fn validate(bytes: &[u8], build: &str) -> Result<String> {
    let verified = crate::package::codec::verify(&mut &bytes[..])?;
    if crate::identifier::encode(&verified.digest) != build {
        return Err("build content hash mismatch".into());
    }
    Ok(verified.name)
}
