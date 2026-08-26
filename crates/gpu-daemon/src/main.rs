//! gpu-daemon 二进制入口:解析 CLI 参数 → 组装 DaemonConfig → run。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use gpu_daemon::{DaemonConfig, ProviderKind};
use tracing_subscriber::EnvFilter;

/// GPU 资源调度守护进程
#[derive(Debug, Parser)]
#[command(name = "gpu-daemon", version, about)]
struct Args {
    /// gRPC 监听 socket 路径
    #[arg(long, default_value = default_socket())]
    socket_path: PathBuf,

    /// 设备发现后端
    #[arg(long, value_enum, default_value_t = ProviderArg::Null)]
    provider: ProviderArg,

    /// 监控轮询间隔(秒)
    #[arg(long, default_value_t = 5)]
    poll_interval_secs: u64,

    /// Prometheus /metrics 端口(0 = 关闭)
    #[arg(long, default_value_t = 9900)]
    metrics_port: u16,

    /// 排队超时秒数(0 = 不限制,默认 1h,ADR-2)
    #[arg(long, default_value_t = 3600)]
    queue_timeout_secs: u64,

    /// null provider 的模拟设备数量(仅 --provider null 生效,0 = 空)
    #[arg(long, default_value_t = 0)]
    fake_devices: u32,
}

/// 默认 socket 路径:Linux 用 /run,其他平台(开发机)用 /tmp
fn default_socket() -> &'static str {
    if cfg!(target_os = "linux") {
        "/run/gpu-manager/gpu.sock"
    } else {
        "/tmp/gpu-manager/gpu.sock"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ProviderArg {
    /// 无硬件开发后端(模拟设备,无需 NVIDIA 驱动)
    Null,
    /// NVML 真机后端(需要 NVIDIA 驱动)
    Nvml,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = Args::parse();
    if args.provider != ProviderArg::Null && args.fake_devices > 0 {
        tracing::warn!("--fake-devices 仅对 --provider null 生效,已忽略");
    }

    let cfg = DaemonConfig {
        socket_path: args.socket_path,
        provider: match args.provider {
            ProviderArg::Null => ProviderKind::Null,
            ProviderArg::Nvml => ProviderKind::Nvml,
        },
        poll_interval: Duration::from_secs(args.poll_interval_secs.max(1)),
        metrics_port: args.metrics_port,
        queue_timeout_secs: args.queue_timeout_secs,
        fake_devices: args.fake_devices,
    };

    gpu_daemon::run(cfg).await.context("daemon 运行失败")
}
