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

    /// M3:JSONL 审计日志路径(默认关闭;建议生产环境启用)
    #[arg(long)]
    audit_log: Option<PathBuf>,

    /// M3:强制层模式(auto 探测 / v2 强制 fail-fast / noop 关闭)
    #[arg(long, value_enum, default_value_t = CgroupArg::Auto)]
    cgroup_mode: CgroupArg,

    /// M4:重启恢复状态文件路径(JSON;默认关闭持久化,进程随 daemon 退出继续跑)
    #[arg(long)]
    state_file: Option<PathBuf>,

    /// M5:每用户同时在飞(Allocated+Running)的最大卡数,0 = 不限(默认)。
    /// 超配额的新请求入队等待,且不阻塞其他用户
    #[arg(long, default_value_t = 0)]
    max_gpus_per_user: u32,

    /// M5:每用户非终态(排队+运行)作业数上限,超限提交即被拒绝,0 = 不限(默认)
    #[arg(long, default_value_t = 0)]
    max_concurrent_jobs_per_user: u32,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum CgroupArg {
    /// 自动探测 cgroup v2,不可用则降级 noop(--cgroup-mode auto 默认)
    Auto,
    /// 强制 v2 隔离,探测失败直接退出(fail-fast)
    V2,
    /// 关闭隔离(测试/开发)
    Noop,
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
        audit_log: args.audit_log,
        cgroup_mode: match args.cgroup_mode {
            CgroupArg::Auto => gpu_daemon::CgroupMode::Auto,
            CgroupArg::V2 => gpu_daemon::CgroupMode::V2,
            CgroupArg::Noop => gpu_daemon::CgroupMode::Noop,
        },
        state_file: args.state_file,
        max_gpus_per_user: args.max_gpus_per_user,
        max_concurrent_jobs_per_user: args.max_concurrent_jobs_per_user,
    };

    gpu_daemon::run(cfg).await.context("daemon 运行失败")
}
