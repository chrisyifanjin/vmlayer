//! Image resolution: what an OCI reference turns into before a backend sees it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::spec::Rootfs;

/// The result of resolving an image reference for one backend. Produced by
/// an [`crate::ImageResolver`]; consumed by the daemon to build a
/// [`super::spec::SandboxSpec`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolvedImage {
    pub rootfs: Rootfs,
    /// `ENTRYPOINT` followed by `CMD` from the image config, if any.
    pub entrypoint: Option<Vec<String>>,
    pub workdir: Option<String>,
    pub env: BTreeMap<String, String>,
}
