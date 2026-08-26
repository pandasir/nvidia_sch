//! gpu-cli —— GPU 资源调度命令行客户端。
//!
//! M1:实现 `ls` / `watch`(只读路径,经 gRPC over Unix Socket 连接 daemon);
//! 作业类子命令(`run`/`jobs`/`queue`/`kill`)在 M2 接入,当前为占位。

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
    /// 提交并运行任务(接入:M2)
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
    /// 查看任务列表(接入:M2)
    Jobs,
    /// 查看排队情况(接入:M2)
    Queue,
    /// 终止任务,仅 root 与 job owner 可操作(接入:M2,ADR-1)
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
        Cmd::Run { .. } => stub("任务提交在 M2 里程碑接入"),
        Cmd::Jobs => stub("任务列表在 M2 里程碑接入"),
        Cmd::Queue => stub("排队信息在 M2 里程碑接入"),
        Cmd::Kill { .. } => stub("kill 在 M2 里程碑接入"),
    }
}

fn stub(msg: &str) -> anyhow::Result<()> {
    eprintln!("⚠️  {msg}");
    Ok(())
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
