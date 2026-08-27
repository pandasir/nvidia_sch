//! gpu-daemon —— GPU 资源调度守护进程。
//!
//! 分层(与 DESIGN.md §4 对应):
//!
//! - state:全局共享状态(调度核心 + 监控快照 + 变更广播 + 进程注册表);
//! - monitor:监控轮询任务(NVML 同步 FFI 在 `spawn_blocking` 中执行);
//! - enforcer:进程启动(软约束注入 + setuid/setgid 降权;pty 见 M3);
//! - cgroup:强制层硬墙(device cgroup 设备过滤 + 进程树回收);
//! - identity:调用者身份(SO_PEERCRED/getpeereid)与授权判定;
//! - audit:JSONL 审计日志(事实来源);
//! - jobs:作业编排(SubmitJob → 拉起 → 退出回收,reaper 周期驱动);
//! - api:gRPC 服务(只读 + 作业闭环 + M3 权限/审计);
//! - metrics:Prometheus /metrics 文本端点(纯 std 实现);
//! - run():组装以上所有组件并进入服务循环。

pub mod api;
pub mod audit;
pub mod cgroup;
pub mod enforcer;
pub mod faults;
pub mod identity;
pub mod jobs;
pub mod metrics;
pub mod monitor;
pub mod recovery;
pub mod state;
#[cfg(target_os = "linux")]
pub mod xid;

pub use cgroup::CgroupMode;
pub use state::AppState;

use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// JSONL 审计日志路径(None = 关闭)
    pub audit_log: Option<PathBuf>,
    /// 强制层模式(auto / v2 / noop)
    pub cgroup_mode: CgroupMode,
    /// M4:重启恢复状态文件(None = 关闭持久化)
    pub state_file: Option<PathBuf>,
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
            unix_minor: None,
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

    // ---- M4 重启恢复:启动早期加载状态文件,重建已分配作业 ----
    // 文件存在即恢复(损坏则拒绝启动,不静默丢状态);恢复出的 Running 进程
    // 以孤儿 pid 接管,reaper 后续负责探测回收。
    let mut restored_running: Vec<(String, recovery::ProcRecord)> = Vec::new();
    let mut restored_owners: Vec<(String, (String, u32))> = Vec::new();
    if let Some(state_file) = &cfg.state_file {
        if state_file.exists() {
            let restored = recovery::load(state_file).with_context(|| {
                format!("状态文件恢复失败(拒绝启动,避免状态丢失): {}", state_file.display())
            })?;
            info!(
                "重启恢复: 从 {} 恢复 {} 个运行中进程,回收 {} 个已失效进程",
                state_file.display(),
                restored.outcome.restored_running,
                restored.outcome.collected_failed
            );
            scheduler = restored.scheduler;
            restored_running = restored.running;
            restored_owners = restored.owners;
        } else {
            info!("状态文件不存在({}),全新启动", state_file.display());
        }
    }

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

    // ---- M3 强制层:device cgroup(启动前置检查,fail-fast) ----
    let cgroup_root = Path::new("/sys/fs/cgroup");
    let (cgroup, cgroup_warn) = cgroup::select_cgroup(cfg.cgroup_mode, cgroup_root)
        .map_err(|e| anyhow::anyhow!("cgroup 前置检查失败: {e}"))?;
    if let Some(warn) = cgroup_warn {
        tracing::warn!("{}", warn);
    }
    match cfg.cgroup_mode {
        CgroupMode::V2 => info!("device cgroup: 已启用(V2,真机硬隔离)"),
        CgroupMode::Noop => info!("device cgroup: noop(无隔离)"),
        CgroupMode::Auto => info!("device cgroup: auto(探测结果见上)"),
    }

    // ---- M3 审计日志(JSONL append-only) ----
    let audit = match cfg.audit_log {
        Some(ref p) => {
            let a = Arc::new(
                crate::audit::AuditLog::new(Some(p.clone()))
                    .with_context(|| format!("审计日志初始化失败: {}", p.display()))?,
            );
            info!("审计日志: {}", p.display());
            a
        }
        None => {
            tracing::warn!("审计日志未启用(建议生产环境 --audit-log)");
            Arc::new(crate::audit::AuditLog::new(None)?)
        }
    };

    let state = AppState::new(
        scheduler,
        discovered,
        audit,
        Arc::from(cgroup),
        cfg.state_file.clone(),
    );

    // 恢复出的孤儿进程接入注册表 + 身份记账(须在 reaper 启动前完成)
    if !restored_running.is_empty() {
        recovery::attach_restored(&state, restored_running);
    }
    if !restored_owners.is_empty() {
        let mut owners = state.job_owners.lock().expect("job_owners 锁");
        owners.extend(restored_owners);
    }

    // ---- 监控轮询 ----
    let provider_arc: std::sync::Arc<dyn DeviceProvider> = std::sync::Arc::from(provider);

    // ---- M4:Xid 关键事件监听(仅 Linux,NVML 后端生效) ----
    #[cfg(target_os = "linux")]
    {
        let xid_state = state.clone();
        let xid_provider = provider_arc.clone();
        let _xid_handle = xid::spawn_xid_watcher(xid_state, xid_provider);
    }

    monitor::spawn_monitor(
        state.clone(),
        provider_arc,
        cfg.poll_interval,
        shutdown_rx.clone(),
    );
    info!("监控轮询已启动 (间隔 {}s)", cfg.poll_interval.as_secs());

    // ---- Reaper:作业进程回收 + 队列推进(M2 闭环驱动) ----
    jobs::spawn_reaper(state.clone(), Duration::from_millis(200), shutdown_rx.clone());
    info!("reaper 已启动(回收已退出进程 / 推进队列)");

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
