//! Content-addressed zstd blobs in the data dir.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use sun_core::BlobHash;

use crate::{Result, StoreError};

/// zstd level: fast, still a good ratio on transcripts and logs.
const LEVEL: i32 = 3;

/// Unique suffix for temp files written by this process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Blob directory: `<dir>/<hash[0..2]>/<hash>.zst`. The hash is the sha256 of
/// the **uncompressed** bytes, so equal content is stored once. Files are
/// written to a temp name and renamed, so a crash never leaves a partial blob.
///
/// Blob I/O runs on Tokio's blocking pool. Put blobs **before** opening a
/// [`crate::Tx`] and store only the hash in it; never do blob I/O inside a
/// transaction.
#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stores `bytes` and returns their hash. Idempotent.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<BlobHash> {
        let dir = self.dir.clone();
        blocking(move || put_sync(&dir, &bytes)).await
    }

    /// Returns the uncompressed bytes, verified against the hash.
    /// A missing blob is [`sun_core::Error::NotFound`].
    pub async fn get(&self, hash: &BlobHash) -> Result<Vec<u8>> {
        let path = path_of(&self.dir, hash);
        let hash = hash.clone();
        blocking(move || {
            let compressed = match fs::read(&path) {
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return Err(StoreError::not_found("blob", hash.as_str()));
                }
                other => other?,
            };
            let bytes = zstd::decode_all(compressed.as_slice())
                .map_err(|_| StoreError::CorruptBlob(hash.as_str().to_owned()))?;
            if hash_of(&bytes)? != hash {
                return Err(StoreError::CorruptBlob(hash.as_str().to_owned()));
            }
            Ok(bytes)
        })
        .await
    }
}

fn put_sync(dir: &Path, bytes: &[u8]) -> Result<BlobHash> {
    let hash = hash_of(bytes)?;
    let path = path_of(dir, &hash);
    if path.exists() {
        return Ok(hash);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&zstd::encode_all(bytes, LEVEL)?)?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(hash)
}

fn hash_of(bytes: &[u8]) -> Result<BlobHash> {
    Ok(BlobHash::new(format!("{:x}", Sha256::digest(bytes)))?)
}

fn path_of(dir: &Path, hash: &BlobHash) -> PathBuf {
    let hex = hash.as_str();
    dir.join(&hex[..2]).join(format!("{hex}.zst"))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_dedupes_and_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().to_path_buf());
        let data = b"hello transcript ".repeat(1000);

        let hash = blobs.put(data.clone()).await.unwrap();
        // sha256 of the uncompressed content, independent of compression.
        assert_eq!(hash, hash_of(&data).unwrap());
        assert_eq!(blobs.put(data.clone()).await.unwrap(), hash);
        assert_eq!(blobs.get(&hash).await.unwrap(), data);
        let path = path_of(dir.path(), &hash);
        assert!(fs::metadata(&path).unwrap().len() < data.len() as u64);

        fs::write(&path, zstd::encode_all(&b"tampered"[..], LEVEL).unwrap()).unwrap();
        assert!(matches!(
            blobs.get(&hash).await,
            Err(StoreError::CorruptBlob(_))
        ));

        let missing = BlobHash::new("0".repeat(64)).unwrap();
        assert!(matches!(
            blobs.get(&missing).await,
            Err(StoreError::Core(sun_core::Error::NotFound(_)))
        ));
    }
}
