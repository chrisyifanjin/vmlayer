use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use vmlayer_core::SandboxId;
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::files_server::Files;

use super::convert::{self, status};
use super::Services;

const CHUNK: usize = 64 * 1024;

fn path_ok(p: &str) -> Result<(), Status> {
    if !p.starts_with('/') {
        return Err(Status::invalid_argument("path must be absolute"));
    }
    Ok(())
}

#[tonic::async_trait]
impl Files for Services {
    type ReadStream = ReceiverStream<Result<pb::FileChunk, Status>>;

    async fn read(
        &self,
        req: Request<pb::ReadFileRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let r = req.into_inner();
        path_ok(&r.path)?;
        let live = self
            .daemon
            .live(&SandboxId(r.sandbox_id))
            .await
            .map_err(status)?;
        let data = live.handle.read_file(&r.path).await.map_err(status)?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            for chunk in data.chunks(CHUNK) {
                if tx
                    .send(Ok(pb::FileChunk {
                        data: chunk.to_vec(),
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn write(
        &self,
        req: Request<Streaming<pb::WriteFileRequest>>,
    ) -> Result<Response<pb::Empty>, Status> {
        let mut inbound = req.into_inner();
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("empty write stream"))??;
        let header = match first.part {
            Some(pb::write_file_request::Part::Header(h)) => h,
            _ => return Err(Status::invalid_argument("first message must be a header")),
        };
        path_ok(&header.path)?;
        let mut buf = BytesMut::new();
        while let Some(msg) = inbound.next().await {
            if let Some(pb::write_file_request::Part::Data(d)) = msg?.part {
                buf.extend_from_slice(&d);
            }
        }
        let live = self
            .daemon
            .live(&SandboxId(header.sandbox_id))
            .await
            .map_err(status)?;
        let mode = if header.mode == 0 { 0o644 } else { header.mode };
        live.handle
            .write_file(&header.path, Bytes::from(buf), mode)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn stat(&self, req: Request<pb::StatRequest>) -> Result<Response<pb::FileInfo>, Status> {
        let r = req.into_inner();
        path_ok(&r.path)?;
        let live = self
            .daemon
            .live(&SandboxId(r.sandbox_id))
            .await
            .map_err(status)?;
        // List the parent and pick the entry; backends need no separate stat.
        let trimmed = r.path.trim_end_matches('/');
        let (parent, _) = trimmed.rsplit_once('/').unwrap_or(("/", trimmed));
        let parent = if parent.is_empty() { "/" } else { parent };
        let entries = live.handle.list_dir(parent).await.map_err(status)?;
        let found = entries
            .iter()
            .find(|e| e.path == trimmed)
            .ok_or_else(|| Status::not_found(format!("no such file: {}", r.path)))?;
        Ok(Response::new(convert::file_info(found)))
    }

    async fn list(
        &self,
        req: Request<pb::ListFilesRequest>,
    ) -> Result<Response<pb::ListFilesResponse>, Status> {
        let r = req.into_inner();
        path_ok(&r.path)?;
        let live = self
            .daemon
            .live(&SandboxId(r.sandbox_id))
            .await
            .map_err(status)?;
        let entries = live.handle.list_dir(&r.path).await.map_err(status)?;
        Ok(Response::new(pb::ListFilesResponse {
            entries: entries.iter().map(convert::file_info).collect(),
        }))
    }

    async fn mkdir(&self, req: Request<pb::MkdirRequest>) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        path_ok(&r.path)?;
        let live = self
            .daemon
            .live(&SandboxId(r.sandbox_id))
            .await
            .map_err(status)?;
        live.handle.mkdir(&r.path).await.map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn remove(&self, req: Request<pb::RemoveRequest>) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        path_ok(&r.path)?;
        let live = self
            .daemon
            .live(&SandboxId(r.sandbox_id))
            .await
            .map_err(status)?;
        live.handle
            .remove(&r.path, r.recursive)
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }
}
