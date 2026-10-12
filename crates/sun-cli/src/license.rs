//! The license: `<data_dir>/license` holds the token (not a secret;
//! Sun Cloud writes it there before boot), verified against Sun's public key.
//! The kernel resolves it on every `GET /entitlement`, so a renewed file
//! takes effect without a restart.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sun_api::Licensing;
use sun_core::entitlement::{Entitlement, EntitlementStatus, verify_license};

/// Sun's ed25519 license public key.
/// ponytail: all zeroes until Sun Cloud generates its keypair, so every
/// token is `Invalid` (Community) unless `SUN_LICENSE_PUBLIC_KEY` names the
/// issuer's key (Sun Cloud's control plane, tests). Replace with the real key.
pub const SUN_LICENSE_PUBLIC_KEY: [u8; 32] = [0; 32];

/// The token's file in the data dir (same name as Sun Cloud's `LICENSE_FILE`).
pub const LICENSE_FILE: &str = "license";

/// `$SUN_LICENSE_PUBLIC_KEY` (64 hex chars) or [`SUN_LICENSE_PUBLIC_KEY`].
pub fn public_key() -> Result<[u8; 32]> {
    match std::env::var("SUN_LICENSE_PUBLIC_KEY") {
        Ok(hex) => parse_hex32(&hex).context("SUN_LICENSE_PUBLIC_KEY"),
        Err(_) => Ok(SUN_LICENSE_PUBLIC_KEY),
    }
}

/// The data dir's license file and the key it verifies against.
pub fn licensing(data_dir: &Path) -> Result<Licensing> {
    Ok(Licensing {
        path: data_dir.join(LICENSE_FILE),
        public_key: public_key()?,
    })
}

/// Verifies `token` and replaces the license file (temp + rename). A token
/// that fails to verify is refused with its reason and changes nothing.
pub fn install(data_dir: &Path, token: &str) -> Result<Entitlement> {
    let licensing = licensing(data_dir)?;
    let token = token.trim();
    verify_license(token, &licensing.public_key)?;
    let temp = data_dir.join("license.tmp");
    std::fs::write(&temp, token)
        .and_then(|()| std::fs::rename(&temp, &licensing.path))
        .with_context(|| format!("writing {}", licensing.path.display()))?;
    Ok(licensing.resolve())
}

/// One line for people: `business (active, licensed to Acme)`.
pub fn describe(entitlement: &Entitlement) -> String {
    let status = match &entitlement.status {
        EntitlementStatus::Unlicensed => "self-hosted, no license".to_owned(),
        EntitlementStatus::Active => "active".to_owned(),
        EntitlementStatus::Grace { ends_at } => {
            format!("license expired, works offline until {ends_at}")
        }
        EntitlementStatus::Expired { ended_at } => format!("license expired {ended_at}"),
        EntitlementStatus::Invalid { reason } => format!("invalid: {reason}"),
    };
    match &entitlement.license {
        Some(license) => format!(
            "{} ({status}; licensed to {})",
            entitlement.plan, license.licensee
        ),
        None => format!("{} ({status})", entitlement.plan),
    }
}

/// 32 bytes from 64 hex chars.
pub fn parse_hex32(hex: &str) -> Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.is_ascii() {
        bail!("a key is 64 hex chars");
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).context("a key is hex")?;
    }
    Ok(key)
}
