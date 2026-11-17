//! 重启恢复(M4,DESIGN §6 恢复 / §10 存储与恢复)。
//!
//! - **持久化**: [`StateFile`] = 调度快照 + 运行中作业的 pid/租约 +
//!   身份记账。原子写入(tmp + rename),在每次状态变更点
//!   ([`persist`] 挂接 submit/kill/cancel/pump/故障处理)刷新。
//! - **恢复**: [`load`] 读文件 → 重建 Scheduler → 逐个校验进程
//!   (`kill(pid,0)` 存活;Linux 上僵尸按已死)—— 存活恢复 `Running`
//!   (以 [`crate::state::ProcHandle::Orphan`] 接管),已死/僵尸按失败回收。
//! - 一致性(DESIGN §10):审计事件先落盘、快照随后更新,事件是事实来源,
//!   快照是恢复加速缓存。daemon 退出**不杀子进程**(M2 起
//!   `kill_on_drop(false)`),这正是重启后能恢复 `Running` 的前提。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use gpu_core::model::{FailureReason, JobStatus};
use gpu_core::scheduler::{Scheduler, SchedulerSnapshot};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::enforcer::SpawnSpec;
use crate::state::{AppState, ProcHandle, RunningProcess};

pub const STATE_FILE_VERSION: u32 = 1;

/// 运行中作业的进程记录(重启后据以校验/接管)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcRecord {
    pub job_id: String,
    pub lease_id: String,
    pub pid: u32,
    pub devices: Vec<String>,
    pub started_at_ms: u64,
}

/// job_id → (username, uid) 身份记账
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub job_id: String,
    pub user: String,
    pub uid: u32,
}

/// 状态文件内容(v1)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    pub written_at_ms: u64,
    pub scheduler: SchedulerSnapshot,
    pub processes: Vec<ProcRecord>,
    pub owners: Vec<OwnerRecord>,
    /// 排队中/待拉起作业的启动规格:job_id -> SpawnSpec(重启恢复后供排队作业被分配时拉起进程)
    #[serde(default)]
    pub specs: HashMap<String, SpawnSpec>,
}

/// 恢复结果
#[derive(Debug, Default, Clone, Copy)]
pub struct RestoreOutcome {
    /// 校验存活、恢复为 Running 的进程数
    pub restored_running: usize,
    /// 已死/僵尸、按失败回收的进程数
    pub collected_failed: usize,
}

/// 恢复出的全部数据(供 daemon 启动组装)
pub struct Restored {
    pub scheduler: Scheduler,
    /// (job_id, 进程记录) —— 存活进程,恢复为 Running
    pub running: Vec<(String, ProcRecord)>,
    /// job_id -> (username, uid)
    pub owners: Vec<(String, (String, u32))>,
    /// job_id -> SpawnSpec
    pub specs: HashMap<String, SpawnSpec>,
    pub outcome: RestoreOutcome,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 抓取当前状态为 [`StateFile`](锁顺序 scheduler → processes → specs → job_owners)
pub fn capture(state: &AppState) -> StateFile {
    let sched = state.scheduler.lock().expect("scheduler 锁");
    let procs = state.processes.lock().expect("processes 锁");
    let specs = state.specs.lock().expect("specs 锁");
    let owners = state.job_owners.lock().expect("job_owners 锁");
    StateFile {
        version: STATE_FILE_VERSION,
        written_at_ms: now_ms(),
        scheduler: sched.snapshot(),
        processes: procs
            .iter()
            .filter_map(|(job_id, rp)| {
                let pid = rp.handle.pid()?;
                let lease = sched.lease(job_id);
                let lease_id = if !rp.lease_id.is_empty() {
                    rp.lease_id.clone()
                } else {
                    lease.map(|l| l.id.clone()).unwrap_or_default()
                };
                Some(ProcRecord {
                    job_id: job_id.clone(),
                    lease_id,
                    pid,
                    devices: lease.map(|l| l.device_uuids.clone()).unwrap_or_default(),
                    started_at_ms: rp.started_at_ms,
                })
            })
            .collect(),
        owners: owners
            .iter()
            .map(|(job_id, (user, uid))| OwnerRecord {
                job_id: job_id.clone(),
                user: user.clone(),
                uid: *uid,
            })
            .collect(),
        specs: specs.clone(),
    }
}

/// 原子写状态文件(tmp + rename,避免半写文件)。尽力而为,失败仅告警。
pub fn persist(state: &AppState) {
    let path = match state.state_file.lock().expect("state_file 锁").clone() {
        Some(p) => p,
        None => return,
    };
    let file = capture(state);
    let json = match serde_json::to_string_pretty(&file) {
        Ok(j) => j,
        Err(e) => {
            warn!("状态序列化失败: {e}");
            return;
        }
    };
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        warn!("状态文件写入失败 {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!("状态文件提交失败 {}: {e}", path.display());
    }
}

/// 从状态文件恢复调度器与进程注册表数据。
///
/// 对每个进程记录做存活校验:
/// - 存活且非僵尸 → 保留(恢复 `Running`,由 reaper 以 Orphan 句柄接管);
/// - 已死/僵尸 → 立即回收:作业 `Failed`,释放设备(下次 pump 推进队列)。
pub fn load(path: &Path) -> anyhow::Result<Restored> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取状态文件失败: {}", path.display()))?;
    let file: StateFile = serde_json::from_str(&content)
        .with_context(|| format!("状态文件解析失败: {}", path.display()))?;
    if file.version != STATE_FILE_VERSION {
        anyhow::bail!(
            "状态文件版本不兼容: 文件 {} vs 支持 {}",
            file.version,
            STATE_FILE_VERSION
        );
    }

    let mut scheduler = Scheduler::from_snapshot(file.scheduler);
    let mut running: Vec<(String, ProcRecord)> = Vec::new();
    let mut collected = 0usize;

    for rec in &file.processes {
        if !pid_alive(rec.pid) {
            debug!(
                "重启恢复: 作业 {} 进程 pid={} 已不存在,按失败回收",
                rec.job_id, rec.pid
            );
            let _ = scheduler.finish(
                &rec.job_id,
                false,
                Some(FailureReason::Other("daemon 重启后进程已不存在".into())),
            );
            collected += 1;
            continue;
        }
        running.push((rec.job_id.clone(), rec.clone()));
    }

    // 兜底对账:若调度快照中处于 Allocated/Running 的作业既不在存活进程列表中,
    // 也不具备可重新拉起的 Allocated+SpawnSpec,则立即回收释放其占用的 GPU,防租约悬挂泄漏
    let running_ids: std::collections::HashSet<&str> =
        running.iter().map(|(id, _)| id.as_str()).collect();
    let dangling: Vec<String> = scheduler
        .jobs()
        .filter(|j| match j.status {
            JobStatus::Running => !running_ids.contains(j.id.as_str()),
            JobStatus::Allocated => {
                !running_ids.contains(j.id.as_str()) && !file.specs.contains_key(&j.id)
            }
            _ => false,
        })
        .map(|j| j.id.clone())
        .collect();
    for job_id in dangling {
        debug!("重启恢复: 作业 {job_id} 缺少存活进程或启动规格,按失败回收并释放设备");
        let _ = scheduler.finish(
            &job_id,
            false,
            Some(FailureReason::Other(
                "daemon 重启后缺少存活进程或启动规格".into(),
            )),
        );
        collected += 1;
    }

    let restored_count = running.len();
    let owners: Vec<(String, (String, u32))> = file
        .owners
        .into_iter()
        .map(|o| (o.job_id, (o.user, o.uid)))
        .collect();

    Ok(Restored {
        scheduler,
        running,
        owners,
        specs: file.specs,
        outcome: RestoreOutcome {
            restored_running: restored_count,
            collected_failed: collected,
        },
    })
}

/// 把恢复出的孤儿进程接入正在构造的 AppState(须在 daemon 启动早期,
/// 尚未启动 reaper 之前调用)。
pub fn attach_restored(state: &Arc<AppState>, running: Vec<(String, ProcRecord)>) {
    if running.is_empty() {
        return;
    }
    let mut procs = state.processes.lock().expect("processes 锁");
    for (job_id, rec) in running {
        procs.insert(
            job_id.clone(),
            RunningProcess {
                handle: ProcHandle::Orphan { pid: rec.pid },
                lease_id: rec.lease_id.clone(),
                started_at_ms: rec.started_at_ms,
            },
        );
        info!(
            "重启恢复: 作业 {} (pid={}) 校验存活,恢复 Running",
            job_id, rec.pid
        );
    }
}

/// 校验进程存活:`kill(pid, 0)`(信号 0 只做存在性检查,不发信号)。
/// Linux 上僵尸进程虽"存在",但已不可救,按已死处理。
fn pid_alive(pid: u32) -> bool {
    let pid_n = nix::unistd::Pid::from_raw(pid as i32);
    if nix::sys::signal::kill(pid_n, None).is_err() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if is_zombie(pid) {
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, rest)| rest.trim_start().starts_with('Z')))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditLog;
    use crate::cgroup::NoopCgroup;
    use gpu_core::model::JobStatus;
    use gpu_core::scheduler::SubmitRequest;
    use std::time::SystemTime;

    fn test_state() -> Arc<AppState> {
        let scheduler = Scheduler::new(Default::default());
        AppState::new(
            scheduler,
            vec![],
            Arc::new(AuditLog::new(None).unwrap()),
            Arc::new(NoopCgroup),
            None,
        )
    }

    #[test]
    fn pid_alive_probe() {
        // 当前进程必然存活
        let me = std::process::id();
        assert!(pid_alive(me));
        // 极大 pid 必然不存在
        assert!(!pid_alive(4_000_000_000));
    }

    #[test]
    fn capture_persist_roundtrip_records_processes() {
        let state = test_state();
        let file = capture(&state);
        assert!(file.processes.is_empty());
        let json = serde_json::to_string(&file).unwrap();
        let back: StateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, STATE_FILE_VERSION);
    }

    /// 构造:1 卡 + 立即分配的作业 + 指定 pid 的进程记录
    fn fake_snapshot_state(pid: u32) -> (String, StateFile) {
        let mut sched = Scheduler::new(Default::default());
        sched.upsert_device(gpu_core::model::DeviceInfo {
            uuid: "gpu-0".into(),
            name: "t".into(),
            pci_bus_id: "".into(),
            memory_total_mib: 1,
            state: gpu_core::model::DeviceState::Free,
        });
        let (job_id, _) = sched
            .submit(SubmitRequest {
                owner: "bob".into(),
                description: "".into(),
                gpus_requested: 1,
                priority: 5,
                queue_timeout_secs: None,
            })
            .unwrap();
        let file = StateFile {
            version: STATE_FILE_VERSION,
            written_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            scheduler: sched.snapshot(),
            processes: vec![ProcRecord {
                job_id: job_id.clone(),
                lease_id: "lease-x".into(),
                pid,
                devices: vec!["gpu-0".into()],
                started_at_ms: 0,
            }],
            owners: vec![OwnerRecord {
                job_id: job_id.clone(),
                user: "bob".into(),
                uid: 1000,
            }],
            specs: HashMap::new(),
        };
        (job_id, file)
    }

    #[test]
    fn load_with_dead_pid_collects_job() {
        let dir = std::env::temp_dir().join(format!("gpu-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let (job_id, file) = fake_snapshot_state(4_000_000_000);
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();

        let restored = load(&path).unwrap();
        assert!(restored.running.is_empty(), "死进程不应恢复 Running");
        assert_eq!(restored.outcome.collected_failed, 1);
        // 该作业已被回收为 Failed;scheduler 由调用方持有,这里用返回的 scheduler 校验
        let job = restored.scheduler.job(&job_id).unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(restored.scheduler.free_count(), 1, "设备应已释放");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_with_alive_pid_restores_running() {
        let dir = std::env::temp_dir().join(format!("gpu-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state2.json");
        let me = std::process::id(); // 当前测试进程="存活进程"
        let (job_id, file) = fake_snapshot_state(me);
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();

        let restored = load(&path).unwrap();
        assert_eq!(restored.outcome.restored_running, 1);
        assert_eq!(restored.running[0].1.pid, me);
        // 恢复的 scheduler:作业保持 Allocated(未回收)
        assert_eq!(restored.scheduler.job(&job_id).unwrap().status, JobStatus::Allocated);
        assert_eq!(restored.owners.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queued_job_specs_persist_and_dangling_allocated_is_collected() {
        let dir = std::env::temp_dir().join(format!("gpu-recovery-specs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state_specs.json");

        let mut sched = Scheduler::new(Default::default());
        sched.upsert_device(gpu_core::model::DeviceInfo {
            uuid: "gpu-0".into(),
            name: "t".into(),
            pci_bus_id: "".into(),
            memory_total_mib: 1,
            state: gpu_core::model::DeviceState::Free,
        });
        // job_a 占用唯一 GPU,但快照中既无进程记录也无 spec(模拟异常悬挂)
        let (job_a, _) = sched
            .submit(SubmitRequest {
                owner: "alice".into(),
                description: "dangling".into(),
                gpus_requested: 1,
                priority: 5,
                queue_timeout_secs: None,
            })
            .unwrap();
        // job_b 排队中,附带完整 SpawnSpec
        let (job_b, _) = sched
            .submit(SubmitRequest {
                owner: "bob".into(),
                description: "queued".into(),
                gpus_requested: 1,
                priority: 5,
                queue_timeout_secs: None,
            })
            .unwrap();
        assert_eq!(sched.job(&job_b).unwrap().status, JobStatus::Queued);

        let mut specs = HashMap::new();
        specs.insert(
            job_b.clone(),
            SpawnSpec {
                command: vec!["/bin/echo".into(), "ok".into()],
                ..Default::default()
            },
        );
        let file = StateFile {
            version: STATE_FILE_VERSION,
            written_at_ms: now_ms(),
            scheduler: sched.snapshot(),
            processes: vec![],
            owners: vec![],
            specs,
        };
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();

        let restored = load(&path).unwrap();
        // 悬挂的 job_a 应被自动回收并释放 gpu-0
        assert_eq!(restored.outcome.collected_failed, 1);
        assert_eq!(
            restored.scheduler.job(&job_a).unwrap().status,
            JobStatus::Failed
        );
        assert_eq!(restored.scheduler.free_count(), 1);
        // 排队中的 job_b 及其 SpawnSpec 完整恢复
        assert_eq!(
            restored.scheduler.job(&job_b).unwrap().status,
            JobStatus::Queued
        );
        assert_eq!(
            restored.specs.get(&job_b).unwrap().command,
            vec!["/bin/echo".to_string(), "ok".to_string()]
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}