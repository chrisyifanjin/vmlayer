//! gRPC surface: one module per service plus conversions. Services are thin;
//! policy lives in `manager`.

pub mod convert;
pub mod files;
pub mod processes;
pub mod sandboxes;
pub mod system;
pub mod volumes;

use std::sync::Arc;

use crate::manager::Daemon;

#[derive(Clone)]
pub struct Services {
    pub daemon: Arc<Daemon>,
}

pub fn router(daemon: Arc<Daemon>) -> tonic::service::Routes {
    use vmlayer_proto::v1::{
        files_server::FilesServer, processes_server::ProcessesServer,
        sandboxes_server::SandboxesServer, system_server::SystemServer,
        volumes_server::VolumesServer,
    };
    let s = Services { daemon };
    tonic::service::Routes::new(SandboxesServer::new(s.clone()))
        .add_service(ProcessesServer::new(s.clone()))
        .add_service(FilesServer::new(s.clone()))
        .add_service(VolumesServer::new(s.clone()))
        .add_service(SystemServer::new(s))
}
