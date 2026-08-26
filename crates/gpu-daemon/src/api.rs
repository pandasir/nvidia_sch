//! gRPC 服务:实现 `gpu.v1.GpuManager`。
//!
//! M1 里程碑实现只读路径:`ListDevices` / `WatchDevices` / `GetMetrics`;
//! 作业相关 RPC(M2:SubmitJob/KillJob/...) 返回 unimplemented。

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use futures_util::{Stream, StreamExt};
use gpu_api::grpc;
use gpu_api::grpc::gpu_manager_server::{GpuManager, GpuManagerServer};
use tokio::net::UnixListener;
use tokio::sync::watch;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tracing::info;

use crate::state::AppState;

type WatchStream = Pin<Box<dyn Stream<Item = Result<grpc::DeviceStatus, Status>> + Send + 'static>>;

/// gRPC 服务实现
pub struct GpuSvc {
    pub state: Arc<AppState>,
}

/// 启动 gRPC server(Unix Domain Socket + 0600 权限)。
/// `shutdown_rx` 翻转时停止监听并退出。
pub async fn serve_grpc(
    socket_path: PathBuf,
    state: Arc<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // UDS 绑定前清理:已存在文件会导致 bind 失败
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    let chmod = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
    };
    if let Err(e) = chmod {
        tracing::warn!("设置 socket 权限失败(可能无需 root): {e}");
    }

    // tonic 0.12 已为 tokio::net::UnixStream 实现 Connected,直接用原始流即可
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    let svc = GpuManagerServer::new(GpuSvc { state });
    let server = Server::builder().add_service(svc);

    tokio::select! {
        r = server.serve_with_incoming(incoming) => {
            r.map_err(anyhow::Error::from).context("gRPC server 退出")
        }
        changed = shutdown_rx.changed() => {
            if changed.is_ok() && *shutdown_rx.borrow() {
                info!("gRPC server 收到退出信号");
            }
            Ok(())
        }
    }
}

#[tonic::async_trait]
impl GpuManager for GpuSvc {
    async fn list_devices(
        &self,
        _request: Request<grpc::ListDevicesRequest>,
    ) -> Result<Response<grpc::ListDevicesResponse>, Status> {
        Ok(Response::new(grpc::ListDevicesResponse {
            devices: self.state.status_list(),
        }))
    }

    type WatchDevicesStream = WatchStream;

    async fn watch_devices(
        &self,
        _request: Request<grpc::WatchDevicesRequest>,
    ) -> Result<Response<Self::WatchDevicesStream>, Status> {
        // 先回放当前快照,再跟随广播
        let initial = self.state.status_list();
        let rx = self.state.tx.subscribe();
        let stream = futures_util::stream::iter(initial.into_iter().map(Ok::<_, Status>)).chain(
            tokio_stream::wrappers::BroadcastStream::new(rx)
                .map(|r| r.map_err(|_| Status::internal("watch 推送过快被丢弃,请重新订阅"))),
        );
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_metrics(
        &self,
        _request: Request<grpc::GetMetricsRequest>,
    ) -> Result<Response<grpc::GetMetricsResponse>, Status> {
        let (queued, running) = self.state.scheduler_counts();
        Ok(Response::new(grpc::GetMetricsResponse {
            devices: self.state.status_list(),
            queued_jobs: queued,
            running_jobs: running,
        }))
    }

    // ---------- 以下为 M2 才接入的作业类 RPC ----------

    async fn submit_job(
        &self,
        _request: Request<grpc::SubmitJobRequest>,
    ) -> Result<Response<grpc::SubmitJobResponse>, Status> {
        Err(Status::unimplemented("SubmitJob 在 M2 里程碑接入"))
    }

    async fn get_job(
        &self,
        _request: Request<grpc::GetJobRequest>,
    ) -> Result<Response<grpc::Job>, Status> {
        Err(Status::unimplemented("GetJob 在 M2 里程碑接入"))
    }

    async fn list_jobs(
        &self,
        _request: Request<grpc::ListJobsRequest>,
    ) -> Result<Response<grpc::ListJobsResponse>, Status> {
        Err(Status::unimplemented("ListJobs 在 M2 里程碑接入"))
    }

    async fn cancel_job(
        &self,
        _request: Request<grpc::CancelJobRequest>,
    ) -> Result<Response<grpc::CancelJobResponse>, Status> {
        Err(Status::unimplemented("CancelJob 在 M2 里程碑接入"))
    }

    async fn kill_job(
        &self,
        _request: Request<grpc::KillJobRequest>,
    ) -> Result<Response<grpc::KillJobResponse>, Status> {
        Err(Status::unimplemented("KillJob 在 M2 里程碑接入"))
    }

    async fn renew_lease(
        &self,
        _request: Request<grpc::RenewLeaseRequest>,
    ) -> Result<Response<grpc::RenewLeaseResponse>, Status> {
        Err(Status::unimplemented("RenewLease 为 v2 预留"))
    }
}
