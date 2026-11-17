//! 全局共享状态:调度核心 + 监控快照 + 设备状态广播 + 作业进程注册表 +
//! 审计日志 + 强制层 cgroup。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use gpu_api::grpc;
use gpu_core::scheduler::Scheduler;
use nvmlx::{DeviceMetrics, DeviceSnapshot};
use tokio::sync::broadcast;

use crate::audit::AuditLog;
use crate::cgroup::DeviceCgroup;
use crate::enforcer::SpawnSpec;

/// 单个设备当前可见状态 = 静态信息 + 最新指标
#[derive(Debug, Clone)]
pub struct MonitorEntry {
    pub info: DeviceSnapshot,
    pub metrics: Option<DeviceMetrics>,
}

/// 运行中进程的句柄:
///
/// - [`ProcHandle::Tokio`]:本进程 spawn 的 `tokio::process::Child`(reaper 用
///   `try_wait` 精确回收);
/// - [`ProcHandle::Orphan`]:daemon 重启恢复得到的外部 pid(本进程不是其父进程,
///   无法 `wait`;用 `kill(pid, 0)` 探测存活 + `SIGKILL` 终止;僵尸由 pid1 收割)。
#[derive(Debug)]
pub enum ProcHandle {
    Tokio(tokio::process::Child),
    Orphan { pid: u32 },
}

impl ProcHandle {
    pub fn pid(&self) -> Option<u32> {
        match self {
            ProcHandle::Tokio(c) => c.id(),
            ProcHandle::Orphan { pid } => Some(*pid),
        }
    }

    /// 探测进程是否已退出。返回值语义:
    /// `Ok(None)` = 仍在运行;
    /// `Ok(Some(Some(true)))` = 已退出且成功;
    /// `Ok(Some(Some(false)))` = 已退出且非零/被信号杀;
    /// `Ok(Some(None))` = 已退出但**退出状态不可考**
    /// (Orphan —— daemon 重启接管的进程,非其父进程无法 wait 获取退出码;
    /// Linux 上僵尸态同样在此列,提前判定回收)。
    pub fn try_wait(&mut self) -> std::io::Result<Option<Option<bool>>> {
        match self {
            ProcHandle::Tokio(c) => match c.try_wait()? {
                Some(status) => Ok(Some(Some(status.success()))),
                None => Ok(None),
            },
            ProcHandle::Orphan { pid } => {
                let pid_n = nix::unistd::Pid::from_raw(*pid as i32);
                if !alive(pid_n) {
                    return Ok(Some(None)); // 已消失:状态不可考
                }
                // Linux:僵尸进程虽"存在",但已不可运行(kill 无意义),按已退出判
                #[cfg(target_os = "linux")]
                if is_zombie(pid_n.as_raw() as u32) {
                    return Ok(Some(None));
                }
                Ok(None)
            }
        }
    }

    /// 立即终止进程(SIGKILL)。Tokio 句柄用 `start_kill`(同步,不 await);
    /// Orphan 直接 `kill(SIGKILL)`,对僵尸返回错误(忽略)。
    pub fn start_kill(&mut self) -> std::io::Result<()> {
        match self {
            ProcHandle::Tokio(c) => c.start_kill(),
            ProcHandle::Orphan { pid } => {
                let pid = nix::unistd::Pid::from_raw(*pid as i32);
                nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL)
                    .map_err(|e| std::io::Error::other(format!("kill({pid}) 失败: {e}")))
            }
        }
    }
}

/// `kill(pid, 0)` 存活探测(信号 0 不发信号,只做存在性检查;
/// zombie 也返回 Ok(true),由调用方另行判定)
fn alive(pid: nix::unistd::Pid) -> bool {
    nix::sys::signal::kill(pid, None).is_ok()
}

/// Linux:读取 /proc/<pid>/stat 状态字段,判定僵尸(Z)。
#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            // 格式: pid (comm) state ...;comm 可能含空格/括号,状态字段在最后一个 ')' 之后
            s.rsplit_once(')').map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

/// 一个正在运行的作业进程(句柄由 reaper 轮询回收,见 [`crate::jobs`])
#[derive(Debug)]
pub struct RunningProcess {
    pub handle: ProcHandle,
    /// 关联的租约 ID(冗余保存于进程句柄旁:当 KillJob/DeviceOffline/LeaseExpired
    /// 先一步移除 Scheduler 内的 Lease 后,reaper 收割已退出进程时仍能据以清理 cgroup)
    pub lease_id: String,
    pub started_at_ms: u64,
}

/// 全局共享状态(daemon 单进程内共享)
pub struct AppState {
    /// 调度核心(单写者:gRPC 层持有互斥)
    pub scheduler: Mutex<Scheduler>,
    /// 最新监控快照:uuid -> 条目
    pub monitor: RwLock<HashMap<String, MonitorEntry>>,
    /// 设备状态变更广播(gRPC WatchDevices 的推送通道)
    pub tx: broadcast::Sender<grpc::DeviceStatus>,
    /// 运行中作业的进程句柄:job_id -> RunningProcess(由 reaper 轮询回收)
    pub processes: Mutex<HashMap<String, RunningProcess>>,
    /// 作业的启动规格:job_id -> SpawnSpec(排队期间也保留,分配后即用;
    /// 进入终态时移除)。调度核心保持纯逻辑,命令/env 等"外设信息"
    /// 由 daemon 这层持有。
    pub specs: Mutex<HashMap<String, SpawnSpec>>,
    /// 作业身份记账:job_id -> (username, uid),供审计与权限核对
    /// (API 层 submit 时写入,终态清理)。
    pub job_owners: Mutex<HashMap<String, (String, u32)>>,
    /// 审计日志(M3)
    pub audit: Arc<AuditLog>,
    /// 强制层:device cgroup(Noop = 无隔离)
    pub cgroup: Arc<dyn DeviceCgroup>,
    /// 重启恢复(M4):状态文件路径(None = 关闭持久化)
    pub state_file: Mutex<Option<PathBuf>>,
}

impl AppState {
    pub fn new(
        scheduler: Scheduler,
        discovered: Vec<DeviceSnapshot>,
        audit: Arc<AuditLog>,
        cgroup: Arc<dyn DeviceCgroup>,
        state_file: Option<PathBuf>,
    ) -> Arc<Self> {
        let mut monitor = HashMap::new();
        for d in discovered {
            monitor.insert(
                d.uuid.clone(),
                MonitorEntry {
                    info: d,
                    metrics: None,
                },
            );
        }
        Arc::new(Self {
            scheduler: Mutex::new(scheduler),
            monitor: RwLock::new(monitor),
            tx: broadcast::channel(64).0,
            processes: Mutex::new(HashMap::new()),
            specs: Mutex::new(HashMap::new()),
            job_owners: Mutex::new(HashMap::new()),
            audit,
            cgroup,
            state_file: Mutex::new(state_file),
        })
    }

    /// 广播当前设备状态快照(逐设备发送,WatchDevices 逐条推送)。
    /// 设备状态变更点(分配/释放)都应显式调用,让 `gpu watch` 及时感知。
    pub fn broadcast_devices(&self) {
        for status in self.status_list() {
            if self.tx.send(status).is_err() {
                // 无订阅者,忽略
                break;
            }
        }
    }

    /// 合并调度状态与监控指标,产出当前全部设备的 gRPC 状态
    pub fn status_list(&self) -> Vec<grpc::DeviceStatus> {
        let sched = self.scheduler.lock().expect("scheduler 锁");
        let mon = self.monitor.read().expect("monitor 锁");
        sched
            .devices()
            .map(|dev| {
                let m = mon.get(&dev.uuid).and_then(|e| e.metrics.clone());
                grpc::DeviceStatus {
                    uuid: dev.uuid.clone(),
                    name: dev.name.clone(),
                    pci_bus_id: dev.pci_bus_id.clone(),
                    memory_total_mib: dev.memory_total_mib,
                    state: match dev.state {
                        gpu_core::model::DeviceState::Free => grpc::DeviceState::Free as i32,
                        gpu_core::model::DeviceState::Allocated => {
                            grpc::DeviceState::Allocated as i32
                        }
                        gpu_core::model::DeviceState::Offline => grpc::DeviceState::Offline as i32,
                    },
                    utilization_percent: m
                        .as_ref()
                        .map(|x| x.utilization_percent as u64)
                        .unwrap_or(0),
                    used_memory_mib: m.as_ref().map(|x| x.used_memory_mib).unwrap_or(0),
                    temperature_c: m.as_ref().map(|x| x.temperature_c as u64).unwrap_or(0),
                }
            })
            .collect()
    }

    /// 调度器汇总计数:返回 (queued, running)
    pub fn scheduler_counts(&self) -> (u32, u32) {
        let sched = self.scheduler.lock().expect("scheduler 锁");
        let running = sched
            .jobs()
            .filter(|j| {
                matches!(
                    j.status,
                    gpu_core::model::JobStatus::Allocated | gpu_core::model::JobStatus::Running
                )
            })
            .count();
        (sched.queued_count() as u32, running as u32)
    }
}
