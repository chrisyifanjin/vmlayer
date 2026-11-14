//! File operations: archive API for content, `stat` execs for metadata.

use std::io::{Cursor, Read};
use std::time::{Duration, UNIX_EPOCH};

use bollard::query_parameters::{
    DownloadFromContainerOptionsBuilder, UploadToContainerOptionsBuilder,
};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use http_body_util::{Either, Full};
use vmlayer_core::{Error, FileInfo, Result};

use crate::engine_err;
use crate::sandbox::DockerSandbox;

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn split_parent(path: &str) -> Result<(&str, &str)> {
    let path = path.trim_end_matches('/');
    if !path.starts_with('/') || path.is_empty() {
        return Err(Error::Guest(format!("path must be absolute: {path:?}")));
    }
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = if parent.is_empty() { "/" } else { parent };
    if name.is_empty() {
        return Err(Error::Guest(format!("path has no file name: {path:?}")));
    }
    Ok((parent, name))
}

pub(crate) async fn read_file(sb: &DockerSandbox, path: &str) -> Result<Bytes> {
    let opts = DownloadFromContainerOptionsBuilder::new()
        .path(path)
        .build();
    let mut stream = sb
        .docker
        .download_from_container(&sb.container_id, Some(opts));
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => buf.extend_from_slice(&b),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Err(Error::Guest(format!("no such file: {path}"))),
            Err(e) => return Err(engine_err(e)),
        }
    }
    let mut archive = tar::Archive::new(Cursor::new(buf.freeze()));
    for entry in archive.entries().map_err(Error::Io)? {
        let mut entry = entry.map_err(Error::Io)?;
        if entry.header().entry_type().is_file() {
            let mut data = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut data).map_err(Error::Io)?;
            return Ok(Bytes::from(data));
        }
        if entry.header().entry_type().is_dir() {
            return Err(Error::Guest(format!("is a directory: {path}")));
        }
    }
    Err(Error::Guest(format!("no such file: {path}")))
}

pub(crate) async fn write_file(
    sb: &DockerSandbox,
    path: &str,
    data: Bytes,
    mode: u32,
) -> Result<()> {
    let (parent, name) = split_parent(path)?;
    mkdir(sb, parent).await?;
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_mtime(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    header.set_cksum();
    builder
        .append_data(&mut header, name, data.as_ref())
        .map_err(Error::Io)?;
    let tar_bytes = builder.into_inner().map_err(Error::Io)?;
    let opts = UploadToContainerOptionsBuilder::new().path(parent).build();
    sb.docker
        .upload_to_container(
            &sb.container_id,
            Some(opts),
            Either::Left(Full::new(Bytes::from(tar_bytes))),
        )
        .await
        .map_err(engine_err)
}

fn parse_stat_line(line: &str) -> Option<FileInfo> {
    // size|hexmode|mtime|path
    let mut parts = line.splitn(4, '|');
    let size = parts.next()?.trim().parse::<u64>().ok()?;
    let mode = u32::from_str_radix(parts.next()?.trim(), 16).ok()?;
    let mtime = parts.next()?.trim().parse::<u64>().ok()?;
    let path = parts.next()?.to_string();
    Some(FileInfo {
        path,
        size,
        is_dir: mode & 0o170000 == 0o040000,
        mode: mode & 0o7777,
        modified_at: UNIX_EPOCH + Duration::from_secs(mtime),
    })
}

pub(crate) async fn list_dir(sb: &DockerSandbox, path: &str) -> Result<Vec<FileInfo>> {
    let script = format!(
        "find {p} -mindepth 1 -maxdepth 1 -exec stat -c '%s|%f|%Y|%n' {{}} +",
        p = sh_quote(path)
    );
    let (code, out, err) = sb
        .run_capture(vec!["/bin/sh".into(), "-c".into(), script])
        .await?;
    if code != 0 {
        return Err(Error::Guest(format!("list {path}: {}", err.trim())));
    }
    let mut entries: Vec<FileInfo> = out.lines().filter_map(parse_stat_line).collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

pub(crate) async fn mkdir(sb: &DockerSandbox, path: &str) -> Result<()> {
    let (code, _, err) = sb
        .run_capture(vec!["mkdir".into(), "-p".into(), path.to_string()])
        .await?;
    if code != 0 {
        return Err(Error::Guest(format!("mkdir {path}: {}", err.trim())));
    }
    Ok(())
}

pub(crate) async fn remove(sb: &DockerSandbox, path: &str, recursive: bool) -> Result<()> {
    let mut argv = vec!["rm".to_string(), "-f".to_string()];
    if recursive {
        argv.push("-r".into());
    }
    argv.push("--".into());
    argv.push(path.to_string());
    let (code, _, err) = sb.run_capture(argv).await?;
    if code != 0 {
        return Err(Error::Guest(format!("remove {path}: {}", err.trim())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_output() {
        let f = parse_stat_line("1234|81a4|1700000000|/work/a.txt").unwrap();
        assert_eq!(f.size, 1234);
        assert!(!f.is_dir);
        assert_eq!(f.mode, 0o644);
        let d = parse_stat_line("4096|41ed|1700000000|/work/dir").unwrap();
        assert!(d.is_dir);
        assert_eq!(d.mode, 0o755);
        assert!(parse_stat_line("garbage").is_none());
    }

    #[test]
    fn splits_parent() {
        assert_eq!(split_parent("/work/a.txt").unwrap(), ("/work", "a.txt"));
        assert_eq!(split_parent("/a").unwrap(), ("/", "a"));
        assert!(split_parent("relative").is_err());
        assert!(split_parent("/").is_err());
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}
