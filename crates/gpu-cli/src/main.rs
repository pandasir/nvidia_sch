//! gpu-cli —— GPU 资源调度命令行客户端。
//!
//! M1:实现 `ls` / `watch`(只读路径,经 gRPC over Unix Socket 连接 daemon);
//! M2:接入 `run` / `jobs` / `queue` / `kill`(作业闭环)。

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use gpu_api::grpc;
use gpu_api::grpc::gpu_manager_client::GpuManagerClient;
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

/// GPU 资源调度命令行客户端
#[derive(Debug, Parser)]
#[command(name = "gpu", version, about)]
struct Cli {
    /// daemon 的 gRPC socket 路径
    #[arg(long, default_value = default_socket(), global = true)]
    socket_path: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// 列出设备与实时状态
    Ls,
    /// 订阅设备状态变更流(Ctrl-C 退出)
    Watch,
    /// 提交并运行任务(阻塞观察至终态;Ctrl-C 退出观察,任务后台继续)
    Run {
        /// 请求的 GPU 数量
        #[arg(long, default_value_t = 1)]
        gpus: u32,
        /// 展示用描述
        #[arg(long, default_value = "")]
        description: String,
        /// 优先级,越大越优先
        #[arg(long, default_value_t = 5)]
        priority: u8,
        /// 命令及参数
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// 查看任务列表
    Jobs,
    /// 查看排队情况
    Queue,
    /// 终止任务,仅 root 与 job owner 可操作(M3 起强制校验,ADR-1)
    Kill { job_id: String },
}

fn default_socket() -> &'static str {
    if cfg!(target_os = "linux") {
        "/run/gpu-manager/gpu.sock"
    } else {
        "/tmp/gpu-manager/gpu.sock"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ls => cmd_ls(&cli.socket_path).await,
        Cmd::Watch => cmd_watch(&cli.socket_path).await,
        Cmd::Run {
            gpus,
            description,
            priority,
            command,
        } => cmd_run(&cli.socket_path, gpus, description, priority, command).await,
        Cmd::Jobs => cmd_jobs(&cli.socket_path).await,
        Cmd::Queue => cmd_queue(&cli.socket_path).await,
        Cmd::Kill { job_id } => cmd_kill(&cli.socket_path, &job_id).await,
    }
}

/// 建立 gRPC over UDS 连接
async fn connect(path: &Path) -> anyhow::Result<GpuManagerClient<Channel>> {
    let path = path.to_path_buf();
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .context(
            "无法连接 gpu-daemon(请先启动,例如: gpu-daemon --provider null --fake-devices 2)",
        )?;
    Ok(GpuManagerClient::new(channel))
}

const HDR: &str =
    "UUID          NAME                 PCI           STATE      MEM(MiB)  UTIL%  TEMP°C";

async fn cmd_ls(path: &Path) -> anyhow::Result<()> {
    let mut client = connect(path).await?;
    let resp = client
        .list_devices(grpc::ListDevicesRequest {})
        .await
        .context("ListDevices 调用失败")?;
    let devices = resp.into_inner().devices;
    if devices.is_empty() {
        println!("(无设备)");
        return Ok(());
    }
    println!("{HDR}");
    for d in &devices {
        println!(
            "{:<14} {:<20} {:<12} {:<10} {}/{:>6}  {:<5}  {}",
            short_uuid(&d.uuid),
            truncate(&d.name, 20),
            d.pci_bus_id,
            state_str(d.state),
            d.used_memory_mib,
            d.memory_total_mib,
            d.utilization_percent,
            d.temperature_c,
        );
    }
    Ok(())
}

async fn cmd_watch(path: &Path) -> anyhow::Result<()> {
    let mut client = connect(path).await?;
    println!("{HDR}");
    let mut stream = client
        .watch_devices(grpc::WatchDevicesRequest {})
        .await
        .context("WatchDevices 调用失败")?
        .into_inner();

    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    Some(Ok(d)) => println!(
                        "{:<14} {:<20} {:<12} {:<10} {}/{:>6}  {:<5}  {}",
                        short_uuid(&d.uuid),
                        truncate(&d.name, 20),
                        d.pci_bus_id,
                        state_str(d.state),
                        d.used_memory_mib,
                        d.memory_total_mib,
                        d.utilization_percent,
                        d.temperature_c,
                    ),
                    Some(Err(e)) => { eprintln!("watch 错误: {e}"); break; }
                    None => { eprintln!("watch 流已关闭"); break; }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Ctrl-C,退出 watch");
                break;
            }
        }
    }
    Ok(())
}

fn short_uuid(uuid: &str) -> String {
    uuid.chars().take(14).collect()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn state_str(state: i32) -> String {
    match state {
        x if x == grpc::DeviceState::Free as i32 => "FREE".into(),
        x if x == grpc::DeviceState::Allocated as i32 => "ALLOCATED".into(),
        _ => "OFFLINE".into(),
    }
}

// ---------- M2:作业闭环子命令 ----------

/// 提交任务,并阻塞观察到终态。
async fn cmd_run(
    path: &Path,
    gpus: u32,
    description: String,
    priority: u8,
    command: Vec<String>,
) -> anyhow::Result<()> {
    if command.is_empty() {
        anyhow::bail!("command 不能为空");
    }
    let mut client = connect(path).await?;
    let resp = client
        .submit_job(grpc::SubmitJobRequest {
            submission: Some(grpc::Submission {
                description,
                gpus_requested: gpus,
                priority: priority as u32,
                command,
                env: vec![],
                workdir: "".into(),
            }),
        })
        .await
        .context("SubmitJob 调用失败")?
        .into_inner();

    println!(
        "已提交 job {} (状态 {})",
        resp.job_id,
        job_status_str(resp.status)
    );
    println!(
        "请求 {} 张 GPU;资源不足时自动排队。Ctrl-C 退出观察(任务后台继续运行)",
        gpus
    );
    let job_id = resp.job_id;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!(
                    "\n已退出观察;任务继续运行(gpu jobs 查看 / gpu kill {} 终止)",
                    job_id
                );
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                let job = client
                    .get_job(grpc::GetJobRequest { job_id: job_id.clone() })
                    .await
                    .context("GetJob 调用失败")?
                    .into_inner();
                match job.status {
                    x if x == grpc::JobStatus::Succeeded as i32 => {
                        println!("✅ job {} 成功完成", job.id);
                        return Ok(());
                    }
                    x if x == grpc::JobStatus::Failed as i32 => {
                        eprintln!(
                            "❌ job {} 失败: {}",
                            job.id,
                            failure_str(job.failure_reason)
                        );
                        std::process::exit(1);
                    }
                    _ => { /* 继续等待 */ }
                }
            }
        }
    }
}

/// 列出任务
async fn cmd_jobs(path: &Path) -> anyhow::Result<()> {
    let mut client = connect(path).await?;
    let resp = client
        .list_jobs(grpc::ListJobsRequest { owner: "".into() })
        .await
        .context("ListJobs 调用失败")?
        .into_inner();
    if resp.jobs.is_empty() {
        println!("(无任务)");
        return Ok(());
    }
    println!(
        "{:<38} {:<10} {:<4} {:<4} {:<12} NOTE",
        "JOB ID", "OWNER", "GPU", "PRI", "STATUS"
    );
    for j in &resp.jobs {
        let note = if j.failure_reason != grpc::FailureReason::Unspecified as i32 {
            format!("失败: {}", failure_str(j.failure_reason))
        } else if j.status == grpc::JobStatus::Running as i32 {
            j.lease
                .as_ref()
                .map(|l| format!("卡: {}", l.device_uuids.join(",")))
                .unwrap_or_default()
        } else {
            String::new()
        };
        println!(
            "{:<38} {:<10} {:<4} {:<4} {:<12} {}",
            j.id,
            truncate(&j.owner, 10),
            j.gpus_requested,
            j.priority,
            job_status_str(j.status),
            note
        );
    }
    Ok(())
}

/// 查看排队情况
async fn cmd_queue(path: &Path) -> anyhow::Result<()> {
    let mut client = connect(path).await?;
    let resp = client
        .list_jobs(grpc::ListJobsRequest { owner: "".into() })
        .await
        .context("ListJobs 调用失败")?
        .into_inner();
    let queued: Vec<_> = resp
        .jobs
        .iter()
        .filter(|j| j.status == grpc::JobStatus::Queued as i32)
        .collect();
    if queued.is_empty() {
        println!("队列为空(所有请求的资源均已满足)");
        return Ok(());
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    println!("{:<38} {:<10} {:<4} {:<8} 已等待", "JOB ID", "OWNER", "GPU", "PRI");
    for j in queued {
        let waited_secs = now_ms.saturating_sub(j.submitted_at_ms) / 1000;
        println!(
            "{:<38} {:<10} {:<4} {:<8} {}s",
            j.id,
            truncate(&j.owner, 10),
            j.gpus_requested,
            j.priority,
            waited_secs
        );
    }
    Ok(())
}

/// 终止任务(运行中)
async fn cmd_kill(path: &Path, job_id: &str) -> anyhow::Result<()> {
    let mut client = connect(path).await?;
    client
        .kill_job(grpc::KillJobRequest {
            job_id: job_id.into(),
        })
        .await
        .context("KillJob 调用失败(排队中的任务请使用 gpu cancel)")?;
    println!("已终止 job {job_id}(对应 GPU 应立即释放)");
    Ok(())
}

fn job_status_str(status: i32) -> String {
    match status {
        x if x == grpc::JobStatus::Submitted as i32 => "SUBMITTED".into(),
        x if x == grpc::JobStatus::Queued as i32 => "QUEUED".into(),
        x if x == grpc::JobStatus::Allocated as i32 => "ALLOCATED".into(),
        x if x == grpc::JobStatus::Running as i32 => "RUNNING".into(),
        x if x == grpc::JobStatus::Succeeded as i32 => "SUCCEEDED".into(),
        x if x == grpc::JobStatus::Failed as i32 => "FAILED".into(),
        _ => "UNKNOWN".into(),
    }
}

fn failure_str(reason: i32) -> String {
    match reason {
        x if x == grpc::FailureReason::Cancelled as i32 => "已被取消".into(),
        x if x == grpc::FailureReason::QueueTimeout as i32 => "排队超时".into(),
        x if x == grpc::FailureReason::Killed as i32 => "已被 Kill".into(),
        x if x == grpc::FailureReason::LeaseExpired as i32 => "租约过期".into(),
        x if x == grpc::FailureReason::DeviceOffline as i32 => "设备故障下线".into(),
        x if x == grpc::FailureReason::ExitedNonzero as i32 => "进程非零退出".into(),
        _ => "其他原因".into(),
    }
}
