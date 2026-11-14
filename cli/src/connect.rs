//! Channel setup mirroring the SDK: unix socket by default, tcp with a token.

use std::path::PathBuf;

use anyhow::Context;
use futures::StreamExt;
use tonic::transport::{Channel, Endpoint, Uri};
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::{
    files_client::FilesClient, processes_client::ProcessesClient,
    sandboxes_client::SandboxesClient, system_client::SystemClient, volumes_client::VolumesClient,
};

pub fn default_endpoint() -> String {
    if let Some(s) = std::env::var_os("VMLAYER_SOCKET") {
        return format!("unix://{}", s.to_string_lossy());
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return format!(
            "unix://{}",
            PathBuf::from(dir).join("vmlayer/vmlayerd.sock").display()
        );
    }
    if let Some(dir) = std::env::var_os("VMLAYER_DATA_DIR") {
        return format!(
            "unix://{}",
            PathBuf::from(dir).join("vmlayerd.sock").display()
        );
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into());
    format!("unix://{}", home.join(".vmlayer/vmlayerd.sock").display())
}

type Interceptor =
    Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send>;
type Svc = tonic::service::interceptor::InterceptedService<Channel, Interceptor>;

pub struct Clients {
    pub sandboxes: SandboxesClient<Svc>,
    pub processes: ProcessesClient<Svc>,
    pub files: FilesClient<Svc>,
    pub volumes: VolumesClient<Svc>,
    pub system: SystemClient<Svc>,
}

pub async fn connect(endpoint: Option<&str>, token: Option<&str>) -> anyhow::Result<Clients> {
    let endpoint = endpoint.map(String::from).unwrap_or_else(default_endpoint);
    let channel = if let Some(path) = endpoint.strip_prefix("unix://") {
        let path = path.to_string();
        Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(&path).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .with_context(|| format!("connecting to {endpoint} (is vmlayerd running?)"))?
    } else {
        let addr = endpoint.strip_prefix("tcp://").unwrap_or(&endpoint);
        Endpoint::try_from(format!("http://{addr}"))?
            .connect()
            .await
            .with_context(|| format!("connecting to {endpoint}"))?
    };
    let auth = token
        .map(|t| format!("Bearer {t}").parse::<tonic::metadata::MetadataValue<_>>())
        .transpose()?;
    let make = move || -> Interceptor {
        let auth = auth.clone();
        Box::new(move |mut req: tonic::Request<()>| {
            if let Some(a) = &auth {
                req.metadata_mut().insert("authorization", a.clone());
            }
            Ok(req)
        })
    };
    Ok(Clients {
        sandboxes: SandboxesClient::with_interceptor(channel.clone(), make()),
        processes: ProcessesClient::with_interceptor(channel.clone(), make()),
        files: FilesClient::with_interceptor(channel.clone(), make()),
        volumes: VolumesClient::with_interceptor(channel.clone(), make()),
        system: SystemClient::with_interceptor(channel, make()),
    })
}

/// `cp local sb:/path` or `cp sb:/path local`. Files only.
pub async fn copy(c: &mut Clients, src: &str, dst: &str) -> anyhow::Result<()> {
    match (split_remote(src), split_remote(dst)) {
        (None, Some((sb, path))) => {
            let data = tokio::fs::read(src)
                .await
                .with_context(|| format!("reading {src}"))?;
            let mode = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tokio::fs::metadata(src).await?.permissions().mode() & 0o7777
                }
                #[cfg(not(unix))]
                {
                    0o644
                }
            };
            let header = pb::WriteFileRequest {
                part: Some(pb::write_file_request::Part::Header(
                    pb::write_file_request::Header {
                        sandbox_id: sb,
                        path,
                        mode,
                    },
                )),
            };
            let chunks = data
                .chunks(64 * 1024)
                .map(|ch| pb::WriteFileRequest {
                    part: Some(pb::write_file_request::Part::Data(ch.to_vec())),
                })
                .collect::<Vec<_>>();
            let msgs = std::iter::once(header).chain(chunks);
            c.files.write(tokio_stream::iter(msgs)).await?;
        }
        (Some((sb, path)), None) => {
            let mut stream = c
                .files
                .read(pb::ReadFileRequest {
                    sandbox_id: sb,
                    path,
                })
                .await?
                .into_inner();
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                data.extend_from_slice(&chunk?.data);
            }
            tokio::fs::write(dst, data)
                .await
                .with_context(|| format!("writing {dst}"))?;
        }
        _ => anyhow::bail!("exactly one side must be SANDBOX:/path"),
    }
    Ok(())
}

fn split_remote(s: &str) -> Option<(String, String)> {
    let (sb, path) = s.split_once(":/")?;
    if sb.is_empty() || sb.contains('/') {
        return None;
    }
    Some((sb.to_string(), format!("/{path}")))
}
