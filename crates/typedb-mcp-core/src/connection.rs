//! Validated construction of TypeDB drivers and compatibility policy.
use crate::{
    config::{AddressSpec, ConnectionSettings},
    error::InternalError,
};
use serde::Serialize;
use std::path::Path;
use typedb_driver::{Addresses, Credentials, DriverOptions, DriverTlsConfig, TypeDBDriver};
pub const DRIVER_VERSION: &str = "3.12.3";
pub const SUPPORTED_SERVER_FLOOR: &str = "3.12.0";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionStatus {
    Verified,
    Unverified,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerVersionInfo {
    pub distribution: String,
    pub version: String,
    pub status: VersionStatus,
}
#[derive(Debug, Clone)]
pub struct DriverBuildSettings {
    pub addresses: Addresses,
    pub options: DriverOptions,
}
impl ConnectionSettings {
    #[allow(clippy::result_large_err)]
    pub fn driver_build_settings(
        &self,
        retries_override: Option<usize>,
    ) -> Result<DriverBuildSettings, InternalError> {
        let addresses = match &self.address {
            AddressSpec::Legacy(a) => Addresses::try_from_address_str(a),
            AddressSpec::Addresses(a) => Addresses::try_from_addresses_str(a),
            AddressSpec::Translation(m) => Addresses::try_from_translation_str(m.clone()),
        }
        .map_err(InternalError::from)?;
        let tls = if self.tls_enabled {
            if let Some(p) = &self.tls_root_ca_path {
                DriverTlsConfig::enabled_with_root_ca(Path::new(p))
            } else {
                Ok(DriverTlsConfig::enabled_with_native_root_ca())
            }
        } else {
            Ok(DriverTlsConfig::disabled())
        }
        .map_err(InternalError::from)?;
        let mut options = DriverOptions::new(tls);
        if let Some(timeout) = self.request_timeout {
            options = options.request_timeout(timeout);
        }
        if let Some(retries) = retries_override.or(self.primary_failover_retries) {
            options = options.primary_failover_retries(retries);
        }
        Ok(DriverBuildSettings { addresses, options })
    }
    #[allow(clippy::result_large_err)]
    pub fn migration_driver_build_settings(&self) -> Result<DriverBuildSettings, InternalError> {
        self.driver_build_settings(Some(0))
    }
}
#[allow(clippy::result_large_err)]
pub async fn connect(
    settings: &ConnectionSettings,
    username: &str,
    password: &str,
) -> Result<TypeDBDriver, InternalError> {
    let b = settings.driver_build_settings(None)?;
    connect_with_build(b, username, password).await
}
/// Dedicated migration-driver connection: identical validation, but
/// `primary_failover_retries` is forced to zero — migration is a single
/// deliberate pass and must never be silently retried by the driver.
#[allow(clippy::result_large_err)]
pub async fn connect_migration(
    settings: &ConnectionSettings,
    username: &str,
    password: &str,
) -> Result<TypeDBDriver, InternalError> {
    let b = settings.migration_driver_build_settings()?;
    connect_with_build(b, username, password).await
}
#[allow(clippy::result_large_err)]
async fn connect_with_build(
    b: DriverBuildSettings,
    username: &str,
    password: &str,
) -> Result<TypeDBDriver, InternalError> {
    let d = TypeDBDriver::new(b.addresses, Credentials::new(username, password), b.options)
        .await
        .map_err(InternalError::from)?;
    let v = d.server_version().await.map_err(InternalError::from)?;
    validate_server_version(v.version())?;
    Ok(d)
}
#[allow(clippy::result_large_err)]
pub fn validate_server_version(version: &str) -> Result<VersionStatus, InternalError> {
    let core = version.split(['-', '+']).next().unwrap_or("");
    let mut it = core.split('.');
    let major = it.next().and_then(|x| x.parse::<u64>().ok());
    let minor = it.next().and_then(|x| x.parse::<u64>().ok());
    if version.contains('-') || major.is_none() || minor.is_none() {
        return Err(InternalError::Config(format!(
            "TypeDB server version '{version}' is unverified; stable version >= {SUPPORTED_SERVER_FLOOR} is required"
        )));
    }
    if (major.unwrap(), minor.unwrap()) < (3, 12) {
        return Err(InternalError::Config(format!(
            "TypeDB server version '{version}' is unsupported; minimum is {SUPPORTED_SERVER_FLOOR}"
        )));
    }
    Ok(if major == Some(3) && minor == Some(12) {
        VersionStatus::Verified
    } else {
        VersionStatus::Unverified
    })
}
#[allow(clippy::result_large_err)]
pub fn server_version_info(
    distribution: &str,
    version: &str,
) -> Result<ServerVersionInfo, InternalError> {
    Ok(ServerVersionInfo {
        distribution: distribution.to_owned(),
        version: version.to_owned(),
        status: validate_server_version(version)?,
    })
}
/// Check the checked-in release pin without inspecting credentials or runtime state.
pub fn resolved_driver_version_matches_lock(manifest: &str, lock: &str) -> bool {
    let requirement = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("typedb-driver ="));
    requirement.is_some_and(|l| l.contains("=3.12.3"))
        && lock.contains("name = \"typedb-driver\"\nversion = \"3.12.3\"")
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    /// AC.8/AC.9: DRIVER_VERSION, the manifest requirement, and the resolved
    /// Cargo.lock entry must agree. This is a checked-in release constant
    /// check, NOT runtime dependency introspection; update all three together
    /// on driver upgrades.
    #[test]
    fn resolved_driver_version_matches_lock_against_checked_in_files() {
        let manifest = include_str!("../../../Cargo.toml");
        let lock = include_str!("../../../Cargo.lock");
        assert!(resolved_driver_version_matches_lock(manifest, lock));
    }
    #[test]
    fn version_policy() {
        assert_eq!(
            validate_server_version("3.12.0").unwrap(),
            VersionStatus::Verified
        );
        assert_eq!(
            validate_server_version("3.13.0").unwrap(),
            VersionStatus::Unverified
        );
        assert!(validate_server_version("3.11.9").is_err());
        assert!(validate_server_version("3.12.0-alpha").is_err());
    }
    #[test]
    fn migration_disables_retries() {
        let s = ConnectionSettings {
            address: AddressSpec::Legacy("127.0.0.1:1729".into()),
            tls_enabled: false,
            tls_root_ca_path: None,
            request_timeout: Some(Duration::from_secs(2)),
            primary_failover_retries: Some(4),
        };
        assert_eq!(
            s.migration_driver_build_settings()
                .unwrap()
                .options
                .primary_failover_retries,
            0
        );
    }
}
