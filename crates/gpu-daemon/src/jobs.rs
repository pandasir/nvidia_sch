//! 作业编排层:把调度核心(纯状态机)与进程执行(Enforcer)接起来,
//! 完成 **SubmitJob → 进程拉起 → 退出回收** 的闭环(M2)与
//! 强制/审计(M3:cgroup 生命周期、setuid 降权、审计事件落盘、Kill 权限协作)。
//!
//! ## 锁纪律(避免死锁)
//!
//! 全局锁顺序恒定:**scheduler → processes/specs/job_owners**,绝不反向嵌套。
//! `tokio::process::Command::spawn`、`Child::try_wait` 与 cgroup 文件操作均为
//! 同步调用,可在持锁状态下安全执行(微秒级,单机 daemon 可接受)。
//!
//! ## 身份
//!
//! 作业 owner 与 uid 由 API 层从 SO_PEERCRED 解析后写入 [`AppState::job_owners`],
//! 本层只读取用于审计与终态清理;Kill/Cancel 的授权(仅 root 与 owner,ADR-1)
//! 在 API 层完成(DESIGN §13)。

use crate::audit::AuditEvent;
use crate::cgroup;
use crate::enforcer::{self, SpawnSpec};
use crate::state::{AppState, ProcHandle, RunningProcess};
use gpu_core::model::{FailureReason, JobStatus};
use gpu_core::scheduler::{Scheduler, SchedulerEvent, SubmitRequest};
use gpu_core::SchedulerError;
use nvmlx::DeviceSnapshot;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tracing::{debug, warn};

/// 当前时间 (epoch ms),与 gpu-core 时钟约定一致
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 提交作业并尝试立即执行。返回 (job_id, 提交后的即时状态:
/// `Running` 已拉起进程 / `Queued` 排队等待)。
///
/// `caller_uid`:调用者真实 uid(API 层自 SO_PEERCRED 解析),
/// 与 owner 一起写入 `job_owners`,供后续审计使用。
pub fn submit_job(
    state: &AppState,
    req: SubmitRequest,
    spec: SpawnSpec,
    caller_uid: u32,
) -> Result<(String, JobStatus), SchedulerError> {
    let owner = req.owner.clone();
    audit_rpc(
        state,
        AuditEvent::SubmitJob,
        &owner,
        caller_uid,
        None,
        &[],
        format!("p{} {}", req.priority, spec.command.join(" ")),
    );
    let mut sched = state.scheduler.lock().expect("scheduler 锁");
    let (job_id, events) = sched.submit(req)?;
    // 先存规格与身份:若立即分配,`spawn_one` 需要它们
    state.specs.lock().expect("specs 锁").insert(job_id.clone(), spec);
    state
        .job_owners
        .lock()
        .expect("job_owners 锁")
        .insert(job_id.clone(), (owner, caller_uid));
    audit_sched_events(state, &events);
    if sched.job(&job_id).map(|j| j.status) == Some(JobStatus::Allocated) {
        spawn_one(&mut sched, state, &job_id);
    }
    let status = sched
        .job(&job_id)
        .map(|j| j.status)
        .unwrap_or(JobStatus::Queued);
    drop(sched); // 广播会重入 scheduler 锁(std Mutex 不可重入),必须先释放
    state.broadcast_devices();
    crate::recovery::persist(state);
    Ok((job_id, status))
}

/// 强制终止:置 `Failed(Killed)`、杀进程树(cgroup.kill + 直接信号)、
/// **立即释放设备**,然后推进队列(M2 验收:kill 后卡立即释放)。
///
/// 授权(仅 root 与 job owner)由 API 层先行校验(ADR-1)。
pub fn kill_job(state: &AppState, job_id: &str) -> Result<(), SchedulerError> {
    let (owner, uid, lease_id) = {
        let sched = state.scheduler.lock().expect("scheduler 锁");
        let job = sched
            .job(job_id)
            .ok_or_else(|| SchedulerError::JobNotFound {
                job_id: job_id.into(),
            })?;
        let lease_id = sched.lease(job_id).map(|l| l.id.clone());
        let (u, uid) = state
            .job_owners
            .lock()
            .expect("job_owners 锁")
            .get(job_id)
            .cloned()
            .unwrap_or_else(|| (job.owner.clone(), 0));
        (u, uid, lease_id)
    };
    {
        let mut sched = state.scheduler.lock().expect("scheduler 锁");
        sched.kill(job_id)?;
    }
    audit_rpc(state, AuditEvent::KillJob, &owner, uid, Some(job_id), &[], "");
    // Kill 主动强制终态:补记失败语义(killed 即失败,便于复盘检索)
    audit_rpc(
        state,
        AuditEvent::JobFailed,
        &owner,
        uid,
        Some(job_id),
        &[],
        "被 KillJob 强制终止",
    );

    // 杀整棵进程树:cgroup.kill 优先(含孙进程),再补发直接信号
    if let Some(lid) = &lease_id {
        match state.cgroup.kill_all(lid) {
            Ok(()) => debug!("cgroup.kill 已清理 lease={lid}"),
            Err(e) => warn!("cgroup.kill 失败 lease={lid}: {e}"),
        }
    }
    if let Some(rp) = state.processes.lock().expect("processes 锁").get_mut(job_id) {
        if let Err(e) = rp.handle.start_kill() {
            debug!("直接 kill 进程失败(可能已退出): job={job_id} err={e}");
        }
    }
    if let Some(lid) = &lease_id {
        let _ = state.cgroup.destroy(lid); // 尽力清理
    }
    pump(state);
    crate::recovery::persist(state);
    Ok(())
}

/// 取消排队中的作业(仅 `Queued`;运行中请用 [`kill_job`])。
/// 授权(仅 root 与 owner)由 API 层先行校验。
pub fn cancel_job(state: &AppState, job_id: &str) -> Result<(), SchedulerError> {
    let owner = {
        let sched = state.scheduler.lock().expect("scheduler 锁");
        let job = sched
            .job(job_id)
            .ok_or_else(|| SchedulerError::JobNotFound {
                job_id: job_id.into(),
            })?;
        state
            .job_owners
            .lock()
            .expect("job_owners 锁")
            .get(job_id)
            .map(|(u, _)| u.clone())
            .unwrap_or_else(|| job.owner.clone())
    };
    {
        let mut sched = state.scheduler.lock().expect("scheduler 锁");
        sched.cancel(job_id)?;
    }
    let uid = state
        .job_owners
        .lock()
        .expect("job_owners 锁")
        .get(job_id)
        .map(|(_, u)| *u)
        .unwrap_or(0);
    audit_rpc(state, AuditEvent::CancelJob, &owner, uid, Some(job_id), &[], "");
    pump(state);
    crate::recovery::persist(state);
    Ok(())
}

/// 单次推进:回收 + 调度 + 拉起 + 审计 + cgroup 清理。
/// reaper 周期调用;RPC 在杀/取消后也会调用一次。
pub fn pump(state: &AppState) {
    // 锁顺序:scheduler → processes/specs/job_owners(见模块文档)
    let mut sched = state.scheduler.lock().expect("scheduler 锁");
    let mut changed = false; // M4:本轮是否有状态变更(决定是否持久化)

    // ① 回收已退出的进程(同步 try_wait,不阻塞事件循环)
    //    success:Some(real) = 已知真实退出结果;None = 状态不可考(Orphan 接管)
    let finished: Vec<(String, Option<bool>)> = {
        let mut procs = state.processes.lock().expect("processes 锁");
        let mut done = Vec::new();
        for (job_id, rp) in procs.iter_mut() {
            match rp.handle.try_wait() {
                Ok(Some(status)) => done.push((job_id.clone(), status)),
                Ok(None) => {}
                Err(e) => {
                    warn!("读取进程状态失败 job={job_id}: {e}");
                    done.push((job_id.clone(), Some(false)));
                }
            }
        }
        for (job_id, _) in &done {
            procs.remove(job_id);
        }
        done
    };
    if !finished.is_empty() {
        changed = true;
    }
    for (job_id, success) in finished {
        let (owner, lease_id, devices) = {
            let some_lease = sched.lease(&job_id).map(|l| (l.id.clone(), l.device_uuids.clone()));
            let owner = state
                .job_owners
                .lock()
                .expect("job_owners 锁")
                .get(&job_id)
                .map(|(u, _)| u.clone())
                .unwrap_or_default();
            (owner, some_lease.as_ref().map(|(id, _)| id.clone()), some_lease.map(|(_, d)| d).unwrap_or_default())
        };
        if let Some(job) = sched.job(&job_id) {
            if !job.status.is_terminal() {
                let (reason, detail) = match success {
                    Some(true) => (None, String::new()),
                    Some(false) => (
                        Some(FailureReason::ExitedNonzero),
                        "进程非零退出".to_string(),
                    ),
                    None => (
                        Some(FailureReason::Other(
                            "进程已退出但退出状态未知(daemon 重启后接管,无法 wait 获取退出码)"
                                .into(),
                        )),
                        "退出状态未知(daemon 重启后接管)".to_string(),
                    ),
                };
                // 未知退出状态按失败回收(安全侧:资源释放、不会误报成功)
                let _ = sched.finish(&job_id, success == Some(true), reason);
                state.audit.record(
                    if success == Some(true) {
                        AuditEvent::JobSucceeded
                    } else {
                        AuditEvent::JobFailed
                    },
                    &owner,
                    0,
                    Some(&job_id),
                    &devices,
                    detail,
                );
            }
            // 已被 KillJob 先置终态:跳过(设备早已释放)
        }
        if let Some(lid) = &lease_id {
            let _ = state.cgroup.destroy(lid);
        }
    }

    // ② 推进调度:排队超时取消 / 租约过期回收 / 队首分配
    let events = sched.tick(now_ms());
    audit_sched_events(state, &events);
    if !events.is_empty() {
        changed = true;
    }

    // ③ 拉起"已分配但尚无进程"的作业
    let to_spawn: Vec<String> = {
        let procs = state.processes.lock().expect("processes 锁");
        sched
            .jobs()
            .filter(|j| j.status == JobStatus::Allocated && !procs.contains_key(&j.id))
            .map(|j| j.id.clone())
            .collect()
    };
    if !to_spawn.is_empty() {
        changed = true;
    }
    for job_id in to_spawn {
        spawn_one(&mut sched, state, &job_id);
    }

    // 清理已终态作业的配套信息(规格 + 身份记账)
    state
        .specs
        .lock()
        .expect("specs 锁")
        .retain(|job_id, _| sched.job(job_id).map(|j| !j.status.is_terminal()).unwrap_or(false));
    state
        .job_owners
        .lock()
        .expect("job_owners 锁")
        .retain(|job_id, _| sched.job(job_id).map(|j| !j.status.is_terminal()).unwrap_or(false));

    drop(sched); // 广播会重入 scheduler 锁(std Mutex 不可重入),必须先释放
    state.broadcast_devices();
    if changed {
        crate::recovery::persist(state);
    }
}

/// 启动 reaper 任务:`interval` 周期调用 [`pump`],随 `shutdown_rx` 退出。
///
/// daemon 正常退出时**不会**杀死子进程(`kill_on_drop(false)`):
/// 为 M4 重启恢复预留——恢复时校验 PID 存活并恢复 `Running`。
pub fn spawn_reaper(
    state: Arc<AppState>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    pump(&state);
                    std::thread::yield_now();
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        debug!("reaper 收到退出信号");
                        break;
                    }
                }
            }
        }
    })
}

/// 为单个已分配作业拉起进程:创建隔离 cgroup → spawn(含降权)→ 加入 cgroup
/// → `Running`;任何一步失败立即释放(避免"分配了却跑不起来"的悬挂状态)。
fn spawn_one(sched: &mut Scheduler, state: &AppState, job_id: &str) {
    let Some(lease) = sched.lease(job_id) else {
        fail_spawn(sched, state, job_id, "作业缺少租约,已释放");
        return;
    };
    let Some(spec) = state.specs.lock().expect("specs 锁").get(job_id).cloned() else {
        fail_spawn(sched, state, job_id, "作业缺少启动规格,已释放");
        return;
    };

    // 设备白名单:仅该租约占用的卡(从监控快照中取)
    let lease_snaps: Vec<DeviceSnapshot> = {
        let mon = state.monitor.read().expect("monitor 锁");
        mon.values()
            .filter(|e| lease.device_uuids.contains(&e.info.uuid))
            .map(|e| e.info.clone())
            .collect()
    };
    let whitelist = cgroup::whitelist_for(&lease_snaps);
    if let Err(e) = state.cgroup.create(&lease.id, &whitelist) {
        warn!("cgroup 创建失败 job={job_id} lease={}: {e}", lease.id);
        fail_spawn(sched, state, job_id, &format!("cgroup 隔离失败: {e}"));
        return;
    }

    match enforcer::spawn_with_gpus(&spec, &lease.device_uuids) {
        Ok(child) => {
            let Some(pid) = child.id() else {
                // spawn 成功但拿不到 pid(理论不发生):按启动失败处理
                fail_spawn(sched, state, job_id, "无法获取子进程 pid");
                return;
            };
            // 加入 cgroup 失败(隔离未生效)也视为启动失败
            if let Err(e) = state.cgroup.add_pid(&lease.id, pid) {
                let mut child = child;
                let _ = child.start_kill();
                fail_spawn(
                    sched,
                    state,
                    job_id,
                    &format!("进程加入 cgroup 失败: {e}"),
                );
                return;
            }
            state
                .processes
                .lock()
                .expect("processes 锁")
                .insert(
                    job_id.to_string(),
                    RunningProcess {
                        handle: ProcHandle::Tokio(child),
                        started_at_ms: now_ms(),
                    },
                );
            let _ = sched.mark_running(job_id);
            audit_sched_events(state, &[SchedulerEvent::JobRunning {
                job_id: job_id.to_string(),
            }]);
        }
        Err(e) => {
            warn!("进程启动失败 job={job_id}: {e}");
            fail_spawn(sched, state, job_id, &format!("进程启动失败: {e}"));
        }
    }
}

/// spawn 失败统一收尾:作业 Failed + 释放 + 审计。
///
/// ⚠️ 不在此处调用 `broadcast_devices()`:它内部会重新锁 scheduler
/// (status_list),而调用方(pump/submit_job)仍持有 scheduler 锁 ——
/// std Mutex 不可重入,会死锁。由调用方在释放锁后统一广播。
fn fail_spawn(sched: &mut Scheduler, state: &AppState, job_id: &str, detail: &str) {
    let _ = sched.finish(job_id, false, Some(FailureReason::Other(detail.to_string())));
    state.audit.record(
        AuditEvent::JobFailed,
        &state
            .job_owners
            .lock()
            .expect("job_owners 锁")
            .get(job_id)
            .map(|(u, _)| u.clone())
            .unwrap_or_default(),
        0,
        Some(job_id),
        &[],
        detail,
    );
}

// ---------- 审计辅助 ----------

/// 把调度器事件流转写为审计记录(排队/分配/超时/租约过期等)。
/// pub(crate):供 fault 处理(设备下线)复用。
pub(crate) fn audit_sched_events(state: &AppState, events: &[SchedulerEvent]) {
    for ev in events {
        match ev {
            SchedulerEvent::JobQueued { job_id } => audit_job_event(
                state,
                AuditEvent::JobQueued,
                job_id,
                &[],
                "作业入队",
            ),
            SchedulerEvent::JobAllocated { job_id, lease } => audit_job_event(
                state,
                AuditEvent::JobAllocated,
                job_id,
                &lease.device_uuids,
                "设备已分配",
            ),
            SchedulerEvent::JobRunning { job_id } => audit_job_event(
                state,
                AuditEvent::JobRunning,
                job_id,
                &[],
                "进程已启动",
            ),
            SchedulerEvent::JobFinished { job_id } => audit_job_event(
                state,
                AuditEvent::JobSucceeded,
                job_id,
                &[],
                "作业成功",
            ),
            SchedulerEvent::JobFailed { job_id, reason } => audit_job_event(
                state,
                AuditEvent::JobFailed,
                job_id,
                &[],
                format!("失败: {reason:?}"),
            ),
            SchedulerEvent::JobCancelled { job_id } => audit_job_event(
                state,
                AuditEvent::CancelJob,
                job_id,
                &[],
                "排队中取消",
            ),
            SchedulerEvent::DeviceOffline { uuid } => audit_job_event(state, AuditEvent::DeviceOffline, uuid, &[], "设备下线"),
            SchedulerEvent::DeviceBack { uuid } => {
                debug!("设备恢复上线(审计事件可选): {uuid}");
            }
        }
    }
}

fn audit_job_event(
    state: &AppState,
    event: AuditEvent,
    job_or_dev: &str,
    devices: &[String],
    detail: impl Into<String>,
) {
    let (user, uid) = state
        .job_owners
        .lock()
        .expect("job_owners 锁")
        .get(job_or_dev)
        .cloned()
        .unwrap_or_default();
    state
        .audit
        .record(event, &user, uid, Some(job_or_dev), devices, detail);
}

fn audit_rpc(
    state: &AppState,
    event: AuditEvent,
    user: &str,
    uid: u32,
    job_id: Option<&str>,
    devices: &[String],
    detail: impl Into<String>,
) {
    state.audit.record(event, user, uid, job_id, devices, detail);
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditLog;
    use crate::cgroup::NoopCgroup;
    use gpu_core::scheduler::{SchedulerConfig, SubmitRequest};
    use nvmlx::DeviceSnapshot;

    fn test_state(devices: u32) -> Arc<AppState> {
        let mut scheduler = Scheduler::new(SchedulerConfig {
            queue_timeout_secs: 3600,
            ..Default::default()
        });
        for i in 0..devices {
            scheduler.upsert_device(gpu_core::model::DeviceInfo {
                uuid: format!("gpu-{i}"),
                name: "t".into(),
                pci_bus_id: "".into(),
                memory_total_mib: 16384,
                state: gpu_core::model::DeviceState::Free,
            });
        }
        let discovered: Vec<DeviceSnapshot> = (0..devices)
            .map(|i| DeviceSnapshot {
                uuid: format!("gpu-{i}"),
                name: "t".into(),
                pci_bus_id: "".into(),
                memory_total_mib: 16384,
                unix_minor: None,
            })
            .collect();
        AppState::new(
            scheduler,
            discovered,
            Arc::new(AuditLog::new(None).unwrap()),
            Arc::new(NoopCgroup),
            None,
        )
    }

    /// 提交 1 卡作业并装配成 Running(绕过真实 spawn 的 cgroup 路径,
    /// 直接插入 tokio Child 模拟已启动的进程)
    fn submit_running(state: &Arc<AppState>, cmd: &[&str]) -> (String, std::process::Child) {
        let mut sched = state.scheduler.lock().unwrap();
        let (job_id, _) = sched
            .submit(SubmitRequest {
                owner: "tester".into(),
                description: "".into(),
                gpus_requested: 1,
                priority: 5,
                queue_timeout_secs: None,
            })
            .unwrap();
        // Allocated → 直接放入进程 → mark Running。
        // 用 ProcHandle::Orphan(tokio Child 无法包装已存在进程,
        // 且这精确模拟 daemon 重启恢复后的进程形态:只能 kill(pid,0) 探测)。
        let child = std::process::Command::new(cmd[0])
            .args(&cmd[1..])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let mut procs = state.processes.lock().unwrap();
        procs.insert(
            job_id.clone(),
            RunningProcess {
                handle: ProcHandle::Orphan { pid },
                started_at_ms: now_ms(),
            },
        );
        drop(procs);
        let _ = sched.mark_running(&job_id);
        drop(sched);
        state
            .job_owners
            .lock()
            .unwrap()
            .insert(job_id.clone(), ("tester".into(), 501));
        (job_id, child)
    }

    /// M4 验收"杀进程 → 自动回收":外部 SIGKILL 掉子进程后,
    /// pump 检测到退出 → 作业 Failed → 设备释放 → 排队作业可推进。
    ///
    /// 注:必须运行在 tokio runtime 内 —— pump 的 spawn 路径用
    /// `tokio::process::Command`,离开 runtime 无法启动进程。
    #[tokio::test(flavor = "multi_thread")]
    async fn external_sigkill_auto_recovers_and_frees() {
        let state = test_state(1);
        let (job_a, mut child_a) = submit_running(&state, &["/bin/sleep", "30"]);
        // B 走真实提交路径(submit_job 会存 SpawnSpec,卡被 A 占用 → 排队)
        let (job_b, _) = submit_job(
            &state,
            SubmitRequest {
                owner: "tester".into(),
                description: "".into(),
                gpus_requested: 1,
                priority: 5,
                queue_timeout_secs: None,
            },
            SpawnSpec {
                command: vec!["/bin/sleep".into(), "2".into()],
                ..Default::default()
            },
            501,
        )
        .unwrap();
        assert_eq!(
            state.scheduler.lock().unwrap().job(&job_b).unwrap().status,
            JobStatus::Queued,
            "卡被 A 占用,B 应排队"
        );

        // 杀掉 A 的进程(模拟外部 kill / 崩溃)
        let pid = nix::unistd::Pid::from_raw(
            state
                .processes
                .lock()
                .unwrap()
                .get(&job_a)
                .unwrap()
                .handle
                .pid()
                .unwrap() as i32,
        );
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
        // 测试进程是孤儿进程的直接父:wait 收割(模拟生产环境 pid1/宿主来收割,
        // 生产 daemon 重启后并非其父,无法 wait;实测 Linux 上 zombie 判定生效)
        let status = child_a.wait().expect("wait 收割测试子进程");
        assert!(!status.success());

        // 多次 pump 直到回收(进程退出检测有延迟)
        let mut freed = false;
        for round in 0..50u32 {
            pump(&state);
            let sched = state.scheduler.lock().unwrap();
            let a = sched.job(&job_a).unwrap().status;
            let b = sched.job(&job_b).unwrap().status;
            let bfail = sched.job(&job_b).unwrap().failure.clone();
            eprintln!("round={round} A={a:?} B={b:?} B_failure={bfail:?}");
            if a == JobStatus::Failed && (b == JobStatus::Allocated || b == JobStatus::Running) {
                freed = true;
                break;
            }
            drop(sched);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(freed, "外部杀进程后: A 应 Failed、B 应推进(分配并运行)");
        // B 已拿到 A 释放的卡
        let sched = state.scheduler.lock().unwrap();
        assert_eq!(
            sched.lease(&job_b).unwrap().device_uuids,
            vec!["gpu-0".to_string()],
            "B 应占用 A 释放的 gpu-0"
        );
        drop(sched);
    }
}
