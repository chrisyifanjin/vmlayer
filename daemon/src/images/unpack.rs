//! Applying OCI layers to a directory with whiteout semantics.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use vmlayer_core::Result;

use super::err;
use super::oci::Layer;

const WHITEOUT: &str = ".wh.";
const OPAQUE: &str = ".wh..wh..opq";

fn decompress<'a>(layer: &'a Layer) -> Result<Box<dyn Read + 'a>> {
    let mt = layer.media_type.as_str();
    let bytes: &[u8] = &layer.data;
    if mt.ends_with("+gzip") || mt.ends_with(".gzip") {
        Ok(Box::new(flate2::read::GzDecoder::new(bytes)))
    } else if mt.ends_with("+zstd") || mt.ends_with(".zstd") {
        Ok(Box::new(
            zstd::Decoder::new(bytes).map_err(|e| err(format!("zstd: {e}")))?,
        ))
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::GzDecoder::new(bytes)))
    } else {
        Ok(Box::new(bytes))
    }
}

/// Reject `..` and absolute paths so a hostile layer cannot escape `root`.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Apply one layer's entries to `root`. Returns bytes written.
fn apply_layer(root: &Path, layer: &Layer) -> Result<u64> {
    let reader = decompress(layer)?;
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    // Only root can chown; as root the image keeps its real uids and gids,
    // which a root filesystem needs (see ADR 0009 D2).
    archive.set_preserve_ownerships(is_root());
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(true);
    let mut written = 0u64;
    for entry in archive
        .entries()
        .map_err(|e| err(format!("reading layer: {e}")))?
    {
        let mut entry = entry.map_err(|e| err(format!("reading layer entry: {e}")))?;
        let raw = entry.path().map_err(|e| err(e.to_string()))?.into_owned();
        let Some(rel) = safe_relative(&raw) else {
            continue;
        };
        // Device nodes are provided by devtmpfs in the guest; creating them
        // here needs root and adds nothing.
        let kind = entry.header().entry_type();
        if kind.is_character_special() || kind.is_block_special() {
            continue;
        }
        let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == OPAQUE {
            let dir = root.join(rel.parent().unwrap_or(Path::new("")));
            if dir.is_dir() {
                for child in std::fs::read_dir(&dir)?.flatten() {
                    remove_any(&child.path())?;
                }
            }
            continue;
        }
        if let Some(target) = name.strip_prefix(WHITEOUT) {
            let victim = root
                .join(rel.parent().unwrap_or(Path::new("")))
                .join(target);
            remove_any(&victim)?;
            continue;
        }
        let dest = root.join(&rel);
        // A directory replacing a file or vice versa: clear the old one.
        if let Ok(meta) = std::fs::symlink_metadata(&dest) {
            let incoming_dir = entry.header().entry_type().is_dir();
            if meta.is_dir() != incoming_dir || meta.file_type().is_symlink() {
                remove_any(&dest)?;
            }
        }
        written += entry.size();
        entry.unpack_in(root).map_err(|e| {
            let cause = std::error::Error::source(&e)
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            err(format!("unpacking {}: {e}{cause}", rel.display()))
        })?;
    }
    Ok(written)
}

fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() == 0 }
}

fn remove_any(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Apply all layers in order on a blocking thread. Returns bytes written.
pub async fn apply_layers(root: &Path, layers: &[Layer]) -> Result<u64> {
    let root = root.to_path_buf();
    let layers: Vec<Layer> = layers
        .iter()
        .map(|l| Layer {
            data: l.data.clone(),
            media_type: l.media_type.clone(),
        })
        .collect();
    tokio::task::spawn_blocking(move || {
        if !is_root() {
            tracing::warn!(
                "not running as root: files in the built image will be owned by the daemon's user, \
                 not by the uids recorded in the image"
            );
        }
        let mut total = 0u64;
        for (i, layer) in layers.iter().enumerate() {
            let n = apply_layer(&root, layer)?;
            tracing::debug!(layer = i, bytes = n, "layer applied");
            total += n;
        }
        Ok(total)
    })
    .await
    .map_err(|e| err(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_layer(entries: &[(&str, Option<&[u8]>)]) -> Layer {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_uid(0);
            h.set_gid(0);
            match data {
                Some(d) => {
                    h.set_size(d.len() as u64);
                    h.set_mode(0o644);
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_cksum();
                    b.append_data(&mut h, path, &d[..]).unwrap();
                }
                None => {
                    h.set_size(0);
                    h.set_mode(0o755);
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_cksum();
                    b.append_data(&mut h, path, std::io::empty()).unwrap();
                }
            }
        }
        Layer {
            data: bytes::Bytes::from(b.into_inner().unwrap()),
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
        }
    }

    /// A layer with one entry whose name is written raw, bypassing the tar
    /// builder's own `..` check, the way a hostile image would.
    fn hostile_layer(raw_name: &str, data: &[u8]) -> Layer {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.as_gnu_mut().unwrap().name[..raw_name.len()].copy_from_slice(raw_name.as_bytes());
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append(&h, data).unwrap();
        Layer {
            data: bytes::Bytes::from(b.into_inner().unwrap()),
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
        }
    }

    #[tokio::test]
    async fn path_traversal_entries_are_skipped() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        apply_layers(
            &root,
            &[
                hostile_layer("../escape", b"no"),
                hostile_layer("/abs-escape", b"no"),
            ],
        )
        .await
        .unwrap();
        assert!(!outer.path().join("escape").exists());
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "nothing may be written"
        );
    }

    #[tokio::test]
    async fn layers_whiteouts_and_opaque_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let l1 = tar_layer(&[
            ("etc/", None),
            ("etc/a.conf", Some(b"a")),
            ("etc/b.conf", Some(b"b")),
            ("opt/", None),
            ("opt/x", Some(b"x")),
            ("opt/y", Some(b"y")),
            ("bin/tool", Some(b"old")),
        ]);
        let l2 = tar_layer(&[
            ("etc/.wh.a.conf", Some(b"")),
            ("opt/.wh..wh..opq", Some(b"")),
            ("opt/z", Some(b"z")),
            ("bin/tool", Some(b"new")),
        ]);
        let n = apply_layers(root, &[l1, l2]).await.unwrap();
        assert!(n > 0);
        assert!(!root.join("etc/a.conf").exists());
        assert_eq!(std::fs::read(root.join("etc/b.conf")).unwrap(), b"b");
        assert!(!root.join("opt/x").exists() && !root.join("opt/y").exists());
        assert_eq!(std::fs::read(root.join("opt/z")).unwrap(), b"z");
        assert_eq!(std::fs::read(root.join("bin/tool")).unwrap(), b"new");
    }

    #[tokio::test]
    async fn gzip_layers_are_detected() {
        use std::io::Write;
        let plain = tar_layer(&[("f", Some(b"hello"))]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&plain.data).unwrap();
        let layer = Layer {
            data: bytes::Bytes::from(gz.finish().unwrap()),
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        apply_layers(dir.path(), &[layer]).await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"hello");
    }
}
