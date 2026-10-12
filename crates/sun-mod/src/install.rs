//! Installing a mod package (a folder with `mod.yaml`): check the manifest
//! and the user's approval, verify the publisher's ed25519 signature over the
//! package, copy it in, and return the `mod_install` row.
//!
//! The signature (`mod.sig`, JSON `{public_key, signature}`, hex) covers the
//! SHA-256 [`package_digest`] of every file in the folder except `mod.sig`:
//! relative path, executable bit and contents. Symlinks are refused.

use std::io::Read;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sun_core::model::{ModInstall, ModKind as CoreModKind};
use sun_core::{Error, Result};
use sun_mod_sdk::{ModKind, Permissions};

use crate::manifest::{MANIFEST_FILE, load_manifest};

/// File name of a package's signature inside its folder.
pub const SIGNATURE_FILE: &str = "mod.sig";

/// Domain separation: a signature over a mod package never verifies as
/// anything else (e.g. a license), and vice versa.
const SIGNING_CONTEXT: &[u8] = b"sun-mod-package-v1\n";

/// Domain of a publisher's request to yank a published version.
const YANK_CONTEXT: &[u8] = b"sun-registry-yank-v1\n";

/// First bytes of a packed package ([`pack_package`]).
const ARCHIVE_MAGIC: &[u8] = b"sunpkg-v1\n";

#[derive(Serialize, Deserialize)]
struct SignatureFile {
    public_key: String,
    signature: String,
}

/// Installs the package at `src` into `mods_dir/<name>` (replacing an older
/// install) and returns its `mod_install` row.
///
/// - `trusted`: `Some(keys)` for third-party packages, which must be signed by
///   one of `keys`; `None` for a private mod the user installs knowingly
///   from a local folder (unsigned; it still runs in the sandbox).
/// - `granted`: the permissions the user approved, as listed by
///   [`permission_list`]. Every requested permission must be in it.
///
/// The copy is verified, not the source, so the source changing during
/// install can't slip unsigned files in.
pub fn install_package(
    src: &Path,
    mods_dir: &Path,
    trusted: Option<&[[u8; 32]]>,
    granted: &[String],
) -> Result<ModInstall> {
    let name = load_manifest(src)?.name;
    std::fs::create_dir_all(mods_dir).map_err(io("create mods dir"))?;
    let staging = mods_dir.join(format!(".installing-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = copy_dir(src, &staging)
        .and_then(|()| verify(&staging, trusted, granted))
        .and_then(|record| match record.name == name {
            true => Ok(record),
            false => Err(Error::InvalidInput(
                "mod.yaml changed during install".into(),
            )),
        });
    let record = match result {
        Ok(record) => record,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let target = mods_dir.join(&name);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&staging, &target).map_err(io("move package into place"))?;
    Ok(record)
}

/// Checks the staged copy and builds its row.
fn verify(dir: &Path, trusted: Option<&[[u8; 32]]>, granted: &[String]) -> Result<ModInstall> {
    let manifest = load_manifest(dir)?;
    let requested = permission_list(&manifest.permissions);
    if let Some(missing) = requested.iter().find(|p| !granted.contains(p)) {
        return Err(Error::InvalidInput(format!(
            "mod {}: permission {missing} was not approved",
            manifest.name
        )));
    }
    let digest = package_digest(dir)?;
    let signed_by = match trusted {
        None => None,
        Some(keys) => Some(check_signature(dir, &digest, keys)?),
    };
    Ok(ModInstall {
        name: manifest.name.clone(),
        kind: match manifest.kind {
            ModKind::Plugin => CoreModKind::Plugin,
            ModKind::Extension => CoreModKind::Extension,
        },
        version: manifest.version.clone(),
        manifest: serde_json::to_value(&manifest)
            .map_err(|e| Error::InvalidInput(e.to_string()))?,
        granted_permissions: requested,
        package_digest: Some(hex(&digest)),
        signed_by,
    })
}

/// Returns the signer's hex public key if `mod.sig` is valid for `digest`
/// and the key is trusted.
fn check_signature(dir: &Path, digest: &[u8; 32], trusted: &[[u8; 32]]) -> Result<String> {
    let invalid = |why: &str| Error::InvalidInput(format!("mod signature: {why}"));
    let text = std::fs::read_to_string(dir.join(SIGNATURE_FILE))
        .map_err(|_| invalid("package is not signed (no mod.sig)"))?;
    let file: SignatureFile = serde_json::from_str(&text).map_err(|_| invalid("bad mod.sig"))?;
    let key: [u8; 32] = unhex(&file.public_key).ok_or_else(|| invalid("bad public key"))?;
    if !trusted.contains(&key) {
        return Err(invalid("signed by an untrusted key"));
    }
    let signature: [u8; 64] = unhex(&file.signature).ok_or_else(|| invalid("bad signature"))?;
    VerifyingKey::from_bytes(&key)
        .map_err(|_| invalid("bad public key"))?
        .verify_strict(&signed_message(digest), &Signature::from_bytes(&signature))
        .map_err(|_| invalid("does not match the package"))?;
    Ok(file.public_key.to_ascii_lowercase())
}

/// Checks `dir/mod.sig` against the package and `trusted` keys; returns the
/// signer's hex public key. The same check [`install_package`] runs.
pub fn verify_signature(dir: &Path, trusted: &[[u8; 32]]) -> Result<String> {
    check_signature(dir, &package_digest(dir)?, trusted)
}

fn yank_message(name: &str, version: &str) -> Vec<u8> {
    [YANK_CONTEXT, format!("{name}\n{version}").as_bytes()].concat()
}

/// A publisher's hex signature asking the registry to yank `name@version`.
pub fn sign_yank(name: &str, version: &str, seed: &[u8; 32]) -> String {
    let signature = SigningKey::from_bytes(seed).sign(&yank_message(name, version));
    hex(&signature.to_bytes())
}

/// Whether `signature` (hex) is `key`'s [`sign_yank`] for `name@version`.
pub fn verify_yank(name: &str, version: &str, key: &[u8; 32], signature: &str) -> bool {
    match (VerifyingKey::from_bytes(key), unhex::<64>(signature)) {
        (Ok(key), Some(sig)) => key
            .verify_strict(&yank_message(name, version), &Signature::from_bytes(&sig))
            .is_ok(),
        _ => false,
    }
}

/// Packs the package at `dir` (with its `mod.sig`) into one byte string for
/// upload: `sunpkg-v1\n`, then per file (sorted) the path length (u32 LE),
/// path, executable byte, contents length (u64 LE) and contents.
pub fn pack_package(dir: &Path) -> Result<Vec<u8>> {
    let mut files = Vec::new();
    list_files(dir, Path::new(""), &mut files)?;
    files.sort();
    let mut out = ARCHIVE_MAGIC.to_vec();
    for rel in files {
        let path = dir.join(&rel);
        let meta = std::fs::metadata(&path).map_err(io("read package"))?;
        let contents = std::fs::read(&path).map_err(io("read package"))?;
        let len = u32::try_from(rel.len())
            .map_err(|_| Error::InvalidInput(format!("path too long: {rel}")))?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(rel.as_bytes());
        out.push(u8::from(is_executable(&meta)));
        out.extend_from_slice(&(contents.len() as u64).to_le_bytes());
        out.extend_from_slice(&contents);
    }
    Ok(out)
}

/// Unpacks a [`pack_package`] archive (untrusted input) into the new folder
/// `dir`. Refuses absolute paths, `.`/`..` components, duplicates and
/// truncated archives.
pub fn unpack_package(archive: &[u8], dir: &Path) -> Result<()> {
    let bad = |why: String| Error::InvalidInput(format!("package archive: {why}"));
    let mut rest = archive
        .strip_prefix(ARCHIVE_MAGIC)
        .ok_or_else(|| bad("not a sun package".into()))?;
    std::fs::create_dir(dir).map_err(io("create package dir"))?;
    while !rest.is_empty() {
        let len = u32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap_or_default());
        let rel = std::str::from_utf8(take(&mut rest, u64::from(len))?)
            .map_err(|_| bad("non-UTF-8 path".into()))?;
        let safe = !rel.is_empty()
            && !rel.contains(['\\', '\0'])
            && rel.split('/').all(|c| !matches!(c, "" | "." | ".."));
        if !safe {
            return Err(bad(format!("unsafe path {rel:?}")));
        }
        let executable = take(&mut rest, 1)?[0] == 1;
        let size = u64::from_le_bytes(take(&mut rest, 8)?.try_into().unwrap_or_default());
        let contents = take(&mut rest, size)?;
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io("unpack package"))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| bad(format!("{rel}: {e}")))?;
        std::io::Write::write_all(&mut file, contents).map_err(io("unpack package"))?;
        set_executable(&file, executable)?;
    }
    Ok(())
}

/// Splits the first `n` bytes off `rest`.
fn take<'a>(rest: &mut &'a [u8], n: u64) -> Result<&'a [u8]> {
    let n = usize::try_from(n)
        .ok()
        .filter(|n| *n <= rest.len())
        .ok_or_else(|| Error::InvalidInput("package archive: truncated".into()))?;
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Ok(head)
}

/// Signs the package at `dir` with the publisher key derived from `seed`,
/// writing `mod.sig` (for `sun mod sign` and tests).
pub fn sign_package(dir: &Path, seed: &[u8; 32]) -> Result<()> {
    let key = SigningKey::from_bytes(seed);
    let signature = key.sign(&signed_message(&package_digest(dir)?));
    let file = SignatureFile {
        public_key: hex(key.verifying_key().as_bytes()),
        signature: hex(&signature.to_bytes()),
    };
    let text =
        serde_json::to_string_pretty(&file).map_err(|e| Error::InvalidInput(e.to_string()))?;
    std::fs::write(dir.join(SIGNATURE_FILE), text).map_err(io("write mod.sig"))
}

/// The public key for a signing-key `seed`.
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

fn signed_message(digest: &[u8; 32]) -> Vec<u8> {
    [SIGNING_CONTEXT, digest.as_slice()].concat()
}

/// SHA-256 over the package's files (sorted by relative path), excluding
/// `mod.sig`. Each file adds `path \0 exec-bit len contents`.
pub fn package_digest(dir: &Path) -> Result<[u8; 32]> {
    let mut files = Vec::new();
    list_files(dir, Path::new(""), &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for rel in files.iter().filter(|rel| rel.as_str() != SIGNATURE_FILE) {
        let path = dir.join(rel);
        let mut file = std::fs::File::open(&path).map_err(io("read package"))?;
        let meta = file.metadata().map_err(io("read package"))?;
        hasher.update(rel.as_bytes());
        hasher.update([0, u8::from(is_executable(&meta))]);
        hasher.update(meta.len().to_le_bytes());
        let mut buf = [0u8; 64 * 1024];
        loop {
            match file.read(&mut buf).map_err(io("read package"))? {
                0 => break,
                n => hasher.update(&buf[..n]),
            }
        }
    }
    Ok(hasher.finalize().into())
}

/// Collects relative file paths (with `/`), refusing symlinks and non-UTF-8 names.
fn list_files(root: &Path, rel: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(root.join(rel)).map_err(io("read package"))? {
        let entry = entry.map_err(io("read package"))?;
        let rel = rel.join(entry.file_name());
        let kind = entry.file_type().map_err(io("read package"))?;
        let name = rel
            .to_str()
            .ok_or_else(|| Error::InvalidInput(format!("non-UTF-8 file name {}", rel.display())))?;
        if kind.is_symlink() {
            return Err(Error::InvalidInput(format!(
                "package contains a symlink: {name}"
            )));
        } else if kind.is_dir() {
            list_files(root, &rel, out)?;
        } else {
            out.push(name.replace('\\', "/"));
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    let mut files = Vec::new();
    list_files(src, Path::new(""), &mut files)?;
    if !files.iter().any(|f| f == MANIFEST_FILE) {
        return Err(Error::InvalidInput(format!(
            "{} has no mod.yaml",
            src.display()
        )));
    }
    for rel in files {
        let to: PathBuf = dst.join(&rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io("copy package"))?;
        }
        std::fs::copy(src.join(&rel), &to).map_err(io("copy package"))?;
    }
    Ok(())
}

/// Requested permissions as `kind:value` strings, the unit the user approves
/// and `mod_install.granted_permissions` stores.
pub fn permission_list(p: &Permissions) -> Vec<String> {
    let groups = [
        ("network", &p.network),
        ("secret", &p.secrets),
        ("device", &p.devices),
        ("path", &p.paths),
        ("process", &p.processes),
        ("plugin", &p.plugins),
    ];
    groups
        .into_iter()
        .flat_map(|(kind, values)| values.iter().map(move |v| format!("{kind}:{v}")))
        .collect()
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(file: &std::fs::File, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io("unpack package"))
}

#[cfg(not(unix))]
fn set_executable(_file: &std::fs::File, _executable: bool) -> Result<()> {
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e| Error::Unavailable(format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLISHER: [u8; 32] = [7; 32];

    fn package(root: &Path) -> PathBuf {
        let src = root.join("src");
        std::fs::create_dir_all(src.join("bin")).unwrap_or_else(|e| panic!("{e}"));
        let yaml = "name: demo\nkind: plugin\nversion: 0.1.0\napi: '>=1.0 <2.0'\n\
                    entry: { command: ./bin/demo }\npermissions: { network: [api.example.com] }\n";
        std::fs::write(src.join(MANIFEST_FILE), yaml).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(src.join("bin/demo"), "#!/bin/sh\n").unwrap_or_else(|e| panic!("{e}"));
        src
    }

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("sun-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn install_verifies_signature_and_records_it() {
        let root = root("ok");
        let src = package(&root);
        let mods = root.join("mods");
        let granted = vec!["network:api.example.com".to_owned()];
        sign_package(&src, &PUBLISHER).unwrap_or_else(|e| panic!("{e}"));

        let row = install_package(&src, &mods, Some(&[public_key(&PUBLISHER)]), &granted)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((row.name.as_str(), row.kind), ("demo", CoreModKind::Plugin));
        assert_eq!(row.signed_by, Some(hex(&public_key(&PUBLISHER))));
        assert_eq!(
            row.package_digest,
            Some(hex(&package_digest(&src).unwrap_or_default()))
        );
        assert_eq!(row.granted_permissions, granted);
        assert!(mods.join("demo/bin/demo").is_file());

        // Unapproved permissions are refused.
        let err = install_package(&src, &mods, None, &[]).unwrap_err();
        assert!(err.to_string().contains("network:api.example.com"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_refuses_tampered_unsigned_or_untrusted_packages() {
        let root = root("bad");
        let src = package(&root);
        let mods = root.join("mods");
        let granted = vec!["network:api.example.com".to_owned()];
        let trusted = [public_key(&PUBLISHER)];
        let refused = |why: &str| {
            let err = install_package(&src, &mods, Some(&trusted), &granted).unwrap_err();
            assert!(err.to_string().contains(why), "{why}: {err}");
            assert!(!mods.join("demo").exists(), "{why}: installed anyway");
        };

        refused("not signed");
        sign_package(&src, &[9; 32]).unwrap_or_else(|e| panic!("{e}"));
        refused("untrusted key");
        sign_package(&src, &PUBLISHER).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(src.join("bin/demo"), "#!/bin/sh\nrm -rf ~\n")
            .unwrap_or_else(|e| panic!("{e}"));
        refused("does not match");
        // A packed + unpacked copy keeps the signature valid (exec bits too).
        sign_package(&src, &PUBLISHER).unwrap_or_else(|e| panic!("{e}"));
        let archive = pack_package(&src).unwrap_or_else(|e| panic!("{e}"));
        let copy = root.join("copy");
        unpack_package(&archive, &copy).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            verify_signature(&copy, &trusted).ok(),
            Some(hex(&trusted[0]))
        );
        let evil = [
            ARCHIVE_MAGIC,
            &3u32.to_le_bytes(),
            b"../",
            &[0],
            &0u64.to_le_bytes(),
        ];
        let err = unpack_package(&evil.concat(), &root.join("evil")).unwrap_err();
        assert!(err.to_string().contains("unsafe path"), "{err}");
        let err = unpack_package(&archive[..archive.len() - 1], &root.join("cut")).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
        // Yank requests are bound to the version and the key.
        let yank = sign_yank("demo", "0.1.0", &PUBLISHER);
        assert!(verify_yank("demo", "0.1.0", &trusted[0], &yank));
        assert!(!verify_yank("demo", "0.2.0", &trusted[0], &yank));
        assert!(!verify_yank("demo", "0.1.0", &public_key(&[9; 32]), &yank));

        std::fs::write(src.join("bin/demo"), "#!/bin/sh\nrm -rf ~\n")
            .unwrap_or_else(|e| panic!("{e}"));
        // Unsigned private install still works, and records no signer.
        let row = install_package(&src, &mods, None, &granted).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(row.signed_by, None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
