//! gRPC 服务:实现 `gpu.v1.GpuManager`。
//!
//! M1 实现只读路径(`ListDevices` / `WatchDevices` / `GetMetrics`);
//! M2 接入作业闭环:SubmitJob / GetJob / ListJobs / CancelJob / KillJob
//! (RenewLease 仍为 v2 预留)。

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use futures_util::{Stream, StreamExt};
use gpu_api::grpc;
use gpu_api::grpc::gpu_manager_server::{GpuManager, GpuManagerServer};
use gpu_core::model::{FailureReason, Job, JobStatus};
use gpu_core::scheduler::SubmitRequest;
use gpu_core::SchedulerError;
use tokio::net::UnixListener;
use tokio::sync::watch;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tracing::info;

use crate::enforcer::{RunAs, SpawnSpec};
use crate::identity::{caller_from_request, Caller};
use crate::jobs;
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

    // ---------- M2:作业闭环 ----------

    async fn submit_job(
        &self,
        request: Request<grpc::SubmitJobRequest>,
    ) -> Result<Response<grpc::SubmitJobResponse>, Status> {
        // 身份来自 UDS 对端凭证(SO_PEERCRED/getpeereid),不可伪造(DESIGN §13)
        let caller = caller_from_request(&request);
        let sub = request
            .into_inner()
            .submission
            .ok_or_else(|| Status::invalid_argument("缺少 submission"))?;
        if sub.command.is_empty() {
            return Err(Status::invalid_argument("command 不能为空"));
        }
        if sub.gpus_requested == 0 {
            return Err(Status::invalid_argument("gpus_requested 必须 >= 1"));
        }
        if sub.priority > u8::MAX as u32 {
            return Err(Status::invalid_argument("priority 超出范围 (0-255)"));
        }

        let spec = SpawnSpec {
            command: sub.command,
            env: parse_env(&sub.env).map_err(Status::invalid_argument)?,
            workdir: if sub.workdir.is_empty() {
                None
            } else {
                Some(PathBuf::from(sub.workdir))
            },
            // M3 降权:子进程以调用者身份运行(daemon 为 root 时才真正生效)
            run_as: Some(RunAs {
                uid: caller.uid,
                gid: caller.gid,
            }),
        };
        let req = SubmitRequest {
            owner: caller.username.clone(),
            description: sub.description,
            gpus_requested: sub.gpus_requested,
            priority: sub.priority as u8,
            queue_timeout_secs: None,
        };

        let (job_id, status) = jobs::submit_job(&self.state, req, spec, caller.uid).map_err(map_err)?;
        info!("SubmitJob job={job_id} status={status:?} caller={} (uid={})", caller.username, caller.uid);
        Ok(Response::new(grpc::SubmitJobResponse {
            job_id,
            status: to_grpc_status(status),
        }))
    }

    async fn get_job(
        &self,
        request: Request<grpc::GetJobRequest>,
    ) -> Result<Response<grpc::Job>, Status> {
        let caller = caller_from_request(&request);
        let job_id = request.into_inner().job_id;
        let sched = self.state.scheduler.lock().expect("scheduler 锁");
        let job = sched
            .job(&job_id)
            .ok_or_else(|| Status::not_found(format!("job {job_id} 不存在")))?;
        // 可见性: 仅 root 与 job owner(ADR-1)
        caller.authorize_job(&job.owner).map_err(permission_denied)?;
        Ok(Response::new(to_grpc_job(job, sched.lease(&job_id))))
    }

    async fn list_jobs(
        &self,
        request: Request<grpc::ListJobsRequest>,
    ) -> Result<Response<grpc::ListJobsResponse>, Status> {
        let caller = caller_from_request(&request);
        let requested = request.into_inner().owner;
        let sched = self.state.scheduler.lock().expect("scheduler 锁");
        let mut jobs: Vec<grpc::Job> = sched
            .jobs()
            .filter(|j| {
                if caller.is_root() {
                    // root: 全量或按请求过滤
                    requested.is_empty() || j.owner == requested
                } else {
                    // 普通用户: 强制只看自己的作业
                    j.owner == caller.username
                }
            })
            .map(|j| to_grpc_job(j, sched.lease(&j.id)))
            .collect();
        // 稳定排序:先提交的先展示
        jobs.sort_by_key(|j| (j.submitted_at_ms, j.id.clone()));
        Ok(Response::new(grpc::ListJobsResponse { jobs }))
    }

    async fn cancel_job(
        &self,
        request: Request<grpc::CancelJobRequest>,
    ) -> Result<Response<grpc::CancelJobResponse>, Status> {
        let caller = caller_from_request(&request);
        let job_id = request.into_inner().job_id;
        authorize_owner(&self.state, &caller, &job_id)?;
        jobs::cancel_job(&self.state, &job_id).map_err(map_err)?;
        info!("CancelJob job={job_id} caller={}", caller.username);
        Ok(Response::new(grpc::CancelJobResponse {}))
    }

    async fn kill_job(
        &self,
        request: Request<grpc::KillJobRequest>,
    ) -> Result<Response<grpc::KillJobResponse>, Status> {
        let caller = caller_from_request(&request);
        let job_id = request.into_inner().job_id;
        authorize_owner(&self.state, &caller, &job_id)?;
        jobs::kill_job(&self.state, &job_id).map_err(map_err)?;
        info!("KillJob job={job_id} caller={}", caller.username);
        Ok(Response::new(grpc::KillJobResponse {}))
    }

    async fn renew_lease(
        &self,
        _request: Request<grpc::RenewLeaseRequest>,
    ) -> Result<Response<grpc::RenewLeaseResponse>, Status> {
        Err(Status::unimplemented("RenewLease 为 v2 预留"))
    }
}

// ---------- 映射辅助 ----------

fn map_err(e: SchedulerError) -> Status {
    match e {
        SchedulerError::JobNotFound { .. } | SchedulerError::DeviceNotFound { .. } => {
            Status::not_found(e.to_string())
        }
        SchedulerError::InvalidJobState { .. } => Status::failed_precondition(e.to_string()),
        SchedulerError::InvalidRequest(_) => Status::invalid_argument(e.to_string()),
    }
}

fn permission_denied(msg: String) -> Status {
    Status::permission_denied(msg)
}

/// ADR-1:敏感操作(取消/Kill)仅 root 与 job owner 可执行
///
/// 返回 `Status` 以保留"不存在(404) vs 无权限(403)"语义;
/// 大 Err 类型为既有惯例(见 parse_env 注释),此处需要 Status 细粒度。
#[allow(clippy::result_large_err)]
fn authorize_owner(state: &AppState, caller: &Caller, job_id: &str) -> Result<(), Status> {
    let owner = {
        let sched = state.scheduler.lock().expect("scheduler 锁");
        sched
            .job(job_id)
            .map(|j| j.owner.clone())
            .ok_or_else(|| Status::not_found(format!("job {job_id} 不存在")))?
    };
    caller.authorize_job(&owner).map_err(permission_denied)
}

/// 解析 `KEY=VALUE` 环境变量列表;非法格式返回错误描述(调用方转 Status)
fn parse_env(env: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::with_capacity(env.len());
    for kv in env {
        let Some((k, v)) = kv.split_once('=') else {
            return Err(format!("env 需为 KEY=VALUE 格式,实际: {kv:?}"));
        };
        if k.is_empty() {
            return Err(format!("env 键不能为空: {kv:?}"));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

fn to_grpc_status(s: JobStatus) -> i32 {
    match s {
        JobStatus::Submitted => grpc::JobStatus::Submitted as i32,
        JobStatus::Queued => grpc::JobStatus::Queued as i32,
        JobStatus::Allocated => grpc::JobStatus::Allocated as i32,
        JobStatus::Running => grpc::JobStatus::Running as i32,
        JobStatus::Succeeded => grpc::JobStatus::Succeeded as i32,
        JobStatus::Failed => grpc::JobStatus::Failed as i32,
    }
}

fn to_grpc_failure(f: &FailureReason) -> i32 {
    match f {
        FailureReason::Cancelled => grpc::FailureReason::Cancelled as i32,
        FailureReason::QueueTimeout => grpc::FailureReason::QueueTimeout as i32,
        FailureReason::Killed => grpc::FailureReason::Killed as i32,
        FailureReason::LeaseExpired => grpc::FailureReason::LeaseExpired as i32,
        FailureReason::DeviceOffline => grpc::FailureReason::DeviceOffline as i32,
        FailureReason::ExitedNonzero => grpc::FailureReason::ExitedNonzero as i32,
        FailureReason::Other(_) => grpc::FailureReason::Other as i32,
    }
}

fn to_grpc_job(job: &Job, lease: Option<&gpu_core::model::Lease>) -> grpc::Job {
    grpc::Job {
        id: job.id.clone(),
        owner: job.owner.clone(),
        description: job.description.clone(),
        gpus_requested: job.gpus_requested,
        priority: job.priority as u32,
        status: to_grpc_status(job.status),
        failure_reason: job
            .failure
            .as_ref()
            .map(to_grpc_failure)
            .unwrap_or(grpc::FailureReason::Unspecified as i32),
        submitted_at_ms: job.submitted_at_ms,
        lease: lease.map(|l| grpc::Lease {
            id: l.id.clone(),
            job_id: l.job_id.clone(),
            device_uuids: l.device_uuids.clone(),
            issued_at_ms: l.issued_at_ms,
            expires_at_ms: l.expires_at_ms,
        }),
    }
}