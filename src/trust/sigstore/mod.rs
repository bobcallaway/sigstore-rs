//
// Copyright 2021 The Sigstore Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Helper Structs to interact with the Sigstore TUF repository.
//!
//! The main interaction point is [`SigstoreTrustRoot`], which fetches Rekor's
//! public key and Fulcio's certificate.
//!
//! These can later be given to [`cosign::ClientBuilder`](crate::cosign::ClientBuilder)
//! to enable Fulcio and Rekor integrations.
//!
//! # Example
//!
//! The `SigstoreRootTrust` instance can be created via the [`SigstoreTrustRoot::prefetch`]
//! method.
//!
/// ```rust
/// # use sigstore::trust::sigstore::SigstoreTrustRoot;
/// # use sigstore::errors::Result;
/// # #[tokio::main]
/// # async fn main() -> std::result::Result<(), anyhow::Error> {
/// let repo: Result<SigstoreTrustRoot> = SigstoreTrustRoot::new(None).await;
/// // Now, get Fulcio and Rekor trust roots with the returned `SigstoreRootTrust`
/// # Ok(())
/// # }
/// ```
use futures_util::TryStreamExt;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio_util::bytes::BytesMut;

use sigstore_protobuf_specs::dev::sigstore::{
    common::v1::TimeRange,
    trustroot::v1::{CertificateAuthority, TransparencyLogInstance, TrustedRoot},
};
use tough::TargetName;
use tracing::debug;
use webpki::types::CertificateDer;

mod constants;

use crate::errors::{Result, SigstoreError};
pub use crate::trust::{ManualTrustRoot, TrustRoot};

/// Securely fetches Rekor public key and Fulcio certificates from Sigstore's TUF repository.
#[derive(Debug)]
pub struct SigstoreTrustRoot {
    trusted_root: TrustedRoot,
}

impl SigstoreTrustRoot {
    /// Constructs a new trust repository established by a [tough::Repository].
    pub async fn new(checkout_dir: Option<PathBuf>) -> Result<Self> {
        // These are statically defined and should always parse correctly.
        let metadata_base = url::Url::parse(constants::SIGSTORE_METADATA_BASE)?;
        let target_base = url::Url::parse(constants::SIGSTORE_TARGET_BASE)?;

        let repository = tough::RepositoryLoader::new(
            &constants::static_resource("root.json").expect("Failed to fetch embedded TUF root!"),
            metadata_base,
            target_base,
        )
        .expiration_enforcement(tough::ExpirationEnforcement::Safe)
        .load()
        .await
        .map_err(Box::new)?;

        let trusted_root = {
            let data = Self::fetch_target(&repository, &checkout_dir, "trusted_root.json").await?;
            serde_json::from_slice(&data[..])?
        };

        Ok(Self { trusted_root })
    }

    async fn fetch_target<N>(
        repository: &tough::Repository,
        checkout_dir: &Option<PathBuf>,
        name: N,
    ) -> Result<Vec<u8>>
    where
        N: TryInto<TargetName, Error = tough::error::Error>,
    {
        let name: TargetName = name.try_into().map_err(Box::new)?;
        let local_path = checkout_dir.as_ref().map(|d| d.join(name.raw()));

        let read_remote_target = || async {
            match repository.read_target(&name).await {
                Ok(Some(s)) => Ok(s.try_collect::<BytesMut>().await.map_err(Box::new)?),
                _ => Err(SigstoreError::TufTargetNotFoundError(name.raw().to_owned())),
            }
        };

        // First, try reading the target from disk cache.
        let data = if let Some(Ok(local_data)) = local_path.as_ref().map(std::fs::read) {
            debug!("{}: reading from disk cache", name.raw());
            local_data.to_vec()
        // Try reading the target embedded into the binary.
        } else if let Some(embedded_data) = constants::static_resource(name.raw()) {
            debug!("{}: reading from embedded resources", name.raw());
            embedded_data.to_vec()
        // If all else fails, read the data from the TUF repo.
        } else if let Ok(remote_data) = read_remote_target().await {
            debug!("{}: reading from remote", name.raw());
            remote_data.to_vec()
        } else {
            return Err(SigstoreError::TufTargetNotFoundError(name.raw().to_owned()));
        };

        // Get metadata (hash) of the target and update the disk copy if it doesn't match.
        let Some(target) = repository.targets().signed.targets.get(&name) else {
            return Err(SigstoreError::TufMetadataError(format!(
                "couldn't get metadata for {}",
                name.raw()
            )));
        };

        let data = if Sha256::digest(&data)[..] != target.hashes.sha256[..] {
            debug!("{}: out of date", name.raw());
            read_remote_target().await?.to_vec()
        } else {
            data
        };

        // Write our updated data back to the disk.
        if let Some(local_path) = local_path {
            std::fs::write(local_path, &data)?;
        }

        Ok(data)
    }

    #[inline]
    fn tlog_keys(tlogs: &[TransparencyLogInstance]) -> impl Iterator<Item = &[u8]> {
        tlogs
            .iter()
            .filter_map(|tlog| tlog.public_key.as_ref())
            .filter(|key| is_timerange_valid(key.valid_for.as_ref(), false))
            .filter_map(|key| key.raw_bytes.as_ref())
            .map(|key_bytes| key_bytes.as_slice())
    }

    #[inline]
    fn ca_keys(
        cas: &[CertificateAuthority],
        allow_expired: bool,
    ) -> impl Iterator<Item = &'_ [u8]> {
        cas.iter()
            .filter(move |ca| is_timerange_valid(ca.valid_for.as_ref(), allow_expired))
            .flat_map(|ca| ca.cert_chain.as_ref())
            .flat_map(|chain| chain.certificates.iter())
            .map(|cert| cert.raw_bytes.as_slice())
    }
}

impl crate::trust::TrustRoot for SigstoreTrustRoot {
    /// Fetch Fulcio certificates from the given TUF repository or reuse
    /// the local cache if its contents are not outdated.
    ///
    /// The contents of the local cache are updated when they are outdated.
    fn fulcio_certs(&self) -> Result<Vec<CertificateDer>> {
        // Allow expired certificates: they may have been active when the
        // certificate was used to sign.
        let certs = Self::ca_keys(&self.trusted_root.certificate_authorities, true);
        let certs: Vec<_> = certs
            .map(|c| CertificateDer::from(c).into_owned())
            .collect();

        if certs.is_empty() {
            Err(SigstoreError::TufMetadataError(
                "Fulcio certificates not found".into(),
            ))
        } else {
            Ok(certs)
        }
    }

    /// Fetch Rekor public keys from the given TUF repository or reuse
    /// the local cache if it's not outdated.
    ///
    /// The contents of the local cache are updated when they are outdated.
    fn rekor_keys(&self) -> Result<Vec<&[u8]>> {
        let keys: Vec<_> = Self::tlog_keys(&self.trusted_root.tlogs).collect();

        if keys.len() != 1 {
            Err(SigstoreError::TufMetadataError(
                "Did not find exactly 1 active Rekor key".into(),
            ))
        } else {
            Ok(keys)
        }
    }

    /// Fetch CTFE public keys from the given TUF repository or reuse
    /// the local cache if it's not outdated.
    ///
    /// The contents of the local cache are updated when they are outdated.
    fn ctfe_keys(&self) -> Result<Vec<&[u8]>> {
        let keys: Vec<_> = Self::tlog_keys(&self.trusted_root.ctlogs).collect();

        if keys.is_empty() {
            Err(SigstoreError::TufMetadataError(
                "CTFE keys not found".into(),
            ))
        } else {
            Ok(keys)
        }
    }
}

/// Given a `range`, checks that the the current time is not before `start`. If
/// `allow_expired` is `false`, also checks that the current time is not after
/// `end`.
fn is_timerange_valid(range: Option<&TimeRange>, allow_expired: bool) -> bool {
    let now = chrono::Utc::now().timestamp();

    let start = range.and_then(|r| r.start.as_ref()).map(|t| t.seconds);
    let end = range.and_then(|r| r.end.as_ref()).map(|t| t.seconds);

    match (start, end) {
        // If there was no validity period specified, the key is always valid.
        (None, _) => true,
        // Active: if the current time is before the starting period, we are not yet valid.
        (Some(start), _) if now < start => false,
        // If we want Expired keys, then we don't need to check the end.
        _ if allow_expired => true,
        // If there is no expiry date, the key is valid.
        (_, None) => true,
        // If we have an expiry date, check it.
        (_, Some(end)) => now <= end,
    }
}
