//! gpu-daemon —— GPU 资源调度守护进程。
//!
//! 分层(与 DESIGN.md §4 对应):
//!
//! - [`state`]:全局共享状态(调度核心 + 监控快照 + 变更广播);
//! - [`monitor`]:监控轮询任务(NVML 同步 FFI 在 `spawn_blocking` 中执行);
//! - [`api`]:gRPC 服务(M1 实现 List/Watch/GetMetrics,其余 M2);
//! - [`metrics`]:Prometheus /metrics 文本端点(纯 std 实现);
//! - [`run`](run()):组装以上所有组件并进入服务循环。

pub mod api;
pub mod metrics;
pub mod monitor;
pub mod state;

pub use state::AppState;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use gpu_core::model::{DeviceInfo, DeviceState};
use gpu_core::scheduler::{Scheduler, SchedulerConfig};
use nvmlx::{DeviceProvider, DeviceSnapshot, NullProvider};
use tokio::sync::watch;
use tracing::info;

/// daemon 运行配置
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    /// 设备发现后端
    pub provider: ProviderKind,
    /// 监控轮询间隔
    pub poll_interval: Duration,
    /// Prometheus /metrics 端口(0 = 关闭)
    pub metrics_port: u16,
    /// 排队超时秒数(0 = 不限制)
    pub queue_timeout_secs: u64,
    /// null provider 的模拟设备数量(仅 Null 后端生效,0 = 空)
    pub fake_devices: u32,
}

/// 设备发现后端种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Null,
    Nvml,
}

/// 组装 provider 并做初始设备发现
fn build_provider(cfg: &DaemonConfig) -> anyhow::Result<Box<dyn DeviceProvider>> {
    match cfg.provider {
        ProviderKind::Null => {
            let devices = fake_devices(cfg.fake_devices);
            Ok(Box::new(NullProvider::with_devices(devices)))
        }
        ProviderKind::Nvml => match nvmlx::NvmlProvider::new() {
            Ok(p) => Ok(Box::new(p)),
            Err(e) => anyhow::bail!(
                "NVML 初始化失败: {e} (请确认 NVIDIA 驱动已加载,或改用 --provider null 做无卡开发)"
            ),
        },
    }
}

/// 生成 N 张模拟设备(演示/联调用)
fn fake_devices(n: u32) -> Vec<DeviceSnapshot> {
    (0..n)
        .map(|i| DeviceSnapshot {
            uuid: format!("gpu-fake-{i:02}"),
            name: format!("Fake GPU {i} (sim)"),
            pci_bus_id: format!("0000:{i:02}:00.0"),
            memory_total_mib: 16384,
        })
        .collect()
}

/// 启动全套组件并进入服务循环,Ctrl-C 优雅退出。
pub async fn run(cfg: DaemonConfig) -> anyhow::Result<()> {
    info!(
        "gpu-daemon 启动 (provider={:?}, socket={})",
        cfg.provider,
        cfg.socket_path.display()
    );

    // ---- 初始化调度核心与设备发现 ----
    let provider = build_provider(&cfg)?;
    let discovered = provider.list_devices().context("设备枚举失败")?;
    info!("发现 {} 台设备", discovered.len());

    let mut scheduler = Scheduler::new(SchedulerConfig {
        queue_timeout_secs: cfg.queue_timeout_secs,
        ..Default::default()
    });
    for d in &discovered {
        scheduler.upsert_device(DeviceInfo {
            uuid: d.uuid.clone(),
            name: d.name.clone(),
            pci_bus_id: d.pci_bus_id.clone(),
            memory_total_mib: d.memory_total_mib,
            state: DeviceState::Free,
        });
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = AppState::new(scheduler, discovered);

    // ---- 监控轮询 ----
    let provider_arc: std::sync::Arc<dyn DeviceProvider> = std::sync::Arc::from(provider);
    monitor::spawn_monitor(
        state.clone(),
        provider_arc,
        cfg.poll_interval,
        shutdown_rx.clone(),
    );
    info!("监控轮询已启动 (间隔 {}s)", cfg.poll_interval.as_secs());

    // ---- Prometheus /metrics ----
    if cfg.metrics_port != 0 {
        metrics::spawn_metrics_server(cfg.metrics_port, state.clone())
            .with_context(|| format!("绑定 /metrics 端口 {} 失败", cfg.metrics_port))?;
        info!("/metrics 已监听 http://127.0.0.1:{}", cfg.metrics_port);
    }

    // ---- gRPC server(Unix Socket) ----
    let grpc_handle = tokio::spawn(api::serve_grpc(
        cfg.socket_path.clone(),
        state.clone(),
        shutdown_rx.clone(),
    ));

    info!(
        "gRPC API 已启动 ({}),Ctrl-C 优雅退出",
        cfg.socket_path.display()
    );

    tokio::select! {
        r = grpc_handle => {
            r.context("gRPC server 任务异常退出")??;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("收到 Ctrl-C,优雅关闭");
            let _ = shutdown_tx.send(true);
        }
    }

    // 清理 socket 文件(尽力而为)
    let _ = std::fs::remove_file(&cfg.socket_path);
    info!("已退出");
    Ok(())
}
