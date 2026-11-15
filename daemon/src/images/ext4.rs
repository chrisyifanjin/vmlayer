//! Building an ext4 image from a directory without mounting anything:
//! `mkfs.ext4 -d` (e2fsprogs ≥ 1.43) populates the filesystem directly.

use std::path::Path;

use tokio::process::Command;
use vmlayer_core::Result;

use super::err;

const MIN_MIB: u64 = 256;

pub fn check_tools() -> Result<()> {
    let tool = "mkfs.ext4";
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).exists()))
        .unwrap_or(false);
    if on_path
        || Path::new("/sbin").join(tool).exists()
        || Path::new("/usr/sbin").join(tool).exists()
    {
        Ok(())
    } else {
        Err(err(format!(
            "{tool} not found; install e2fsprogs to build microVM images"
        )))
    }
}

/// Image size for `unpacked` bytes of content: +30% and a floor, in MiB.
pub fn size_mib_for(unpacked: u64) -> u64 {
    let with_slack = unpacked + unpacked / 3;
    (with_slack / (1024 * 1024) + 1).max(MIN_MIB)
}

fn mkfs_bin() -> String {
    for candidate in ["mkfs.ext4", "/sbin/mkfs.ext4", "/usr/sbin/mkfs.ext4"] {
        if candidate.starts_with('/') {
            if Path::new(candidate).exists() {
                return candidate.into();
            }
        } else {
            return candidate.into();
        }
    }
    "mkfs.ext4".into()
}

pub async fn build(dir: &Path, image: &Path, unpacked: u64) -> Result<()> {
    let size = size_mib_for(unpacked);
    let f = tokio::fs::File::create(image).await?;
    f.set_len(size * 1024 * 1024).await?;
    drop(f);
    let out = Command::new(mkfs_bin())
        .args([
            "-q",
            "-F",
            "-E",
            "lazy_itable_init=1,lazy_journal_init=1",
            "-d",
        ])
        .arg(dir)
        .arg(image)
        .output()
        .await
        .map_err(|e| err(format!("mkfs.ext4: {e}")))?;
    if !out.status.success() {
        return Err(err(format!(
            "mkfs.ext4 failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    tracing::info!(image = %image.display(), size_mib = size, unpacked, "ext4 image built");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_have_slack_and_floor() {
        assert_eq!(size_mib_for(0), MIN_MIB);
        assert_eq!(size_mib_for(900 * 1024 * 1024), 1201);
    }

    /// Runs where e2fsprogs exists (Linux CI); skipped elsewhere.
    #[tokio::test]
    async fn builds_an_image_when_mkfs_is_available() {
        if check_tools().is_err() {
            eprintln!("mkfs.ext4 not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/etc")).unwrap();
        std::fs::write(dir.path().join("src/etc/hostname"), "sandbox\n").unwrap();
        let image = dir.path().join("root.ext4");
        build(&dir.path().join("src"), &image, 1024).await.unwrap();
        let meta = std::fs::metadata(&image).unwrap();
        assert_eq!(meta.len(), MIN_MIB * 1024 * 1024);
        let head = std::fs::read(&image).unwrap();
        // ext4 superblock magic at offset 0x438.
        assert_eq!(&head[0x438..0x43a], &[0x53, 0xef]);
    }
}
