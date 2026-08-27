//! 设备故障处理(M4):所有"某台设备不可用"的信号 —— monitor 检测到设备
//! 消失(热插拔/驱动重载)、NVML Xid 事件 —— 统一汇入 [`handle_device_offline`]:
//!
//! 1. 置设备 `Offline`,撤销持有该设备的租约(DESIGN §9:设备下线 → 回收受影响
//!    Lease → 通知 owner,作业置 `Failed(DeviceOffline)`);
//! 2. **杀进程树**(cgroup.kill 优先,含孙进程)+ 直接 SIGKILL —— M4 验收
//!    "杀进程 → 自动回收"的设备故障一侧;
//! 3. 审计 `device_offline` 与逐作业失败;
//! 4. 广播 + 推进队列(释放出的卡立即参与重新分配)。
//!
//! 设备重新出现 → [`handle_device_back`]:`Offline → Free`,重新参与分配。

use std::sync::Arc;

use gpu_core::model::DeviceState;
use tracing::{debug, warn};

use crate::audit::AuditEvent;
use crate::jobs;
use crate::state::AppState;

/// 设备故障下线。幂等:设备不存在或已是 Offline 时直接返回。
pub fn handle_device_offline(state: &Arc<AppState>, uuid: &str) {
    let affected: Vec<(String, String, Vec<String>)> = {
        let mut sched = state.scheduler.lock().expect("scheduler 锁");
        match sched.device_state(uuid) {
            None => {
                debug!("下线信号针对未知设备 {uuid},忽略");
                return;
            }
            Some(DeviceState::Offline) => {
                debug!("设备 {uuid} 已处于 Offline,忽略重复下线信号");
                return;
            }
            Some(_) => {}
        }
        // 先快照受影响作业(含 lease_id;device_offline 会立刻移除租约)
        let affected: Vec<(String, String, Vec<String>)> = sched
            .leases()
            .filter(|l| l.device_uuids.iter().any(|u| u == uuid))
            .map(|l| (l.job_id.clone(), l.id.clone(), l.device_uuids.clone()))
            .collect();
        // 状态机变更:置 Offline + 撤销作业 + 释放设备(事件流含 JobFailed)
        let events = match sched.device_offline(uuid) {
            Ok(ev) => ev,
            Err(e) => {
                warn!("device_offline({uuid}) 失败: {e}");
                return;
            }
        };
        drop(sched); // 审计会重入 scheduler 锁(status_list),先释放
        jobs::audit_sched_events(state, &events);
        affected
    };

    // 杀所有受影响作业的进程树(cgroup.kill + 直接信号;尽力而为)
    for (job_id, lease_id, _devices) in &affected {
        if let Err(e) = state.cgroup.kill_all(lease_id) {
            debug!("cgroup.kill 失败 lease={lease_id}: {e}");
        }
        if let Some(rp) = state
            .processes
            .lock()
            .expect("processes 锁")
            .get_mut(job_id)
        {
            if let Err(e) = rp.handle.start_kill() {
                debug!("设备下线杀进程失败(可能已退出): job={job_id} err={e}");
            }
        }
        let _ = state.cgroup.destroy(lease_id);
    }

    state.audit.record(
        AuditEvent::DeviceOffline,
        "system",
        0,
        None,
        &[uuid.to_string()],
        format!("设备故障下线,受影响作业 {} 个", affected.len()),
    );

    state.broadcast_devices();
    jobs::pump(state);
    crate::recovery::persist(state);
}

/// 设备故障恢复上线(Offline → Free)。幂等。
pub fn handle_device_back(state: &Arc<AppState>, uuid: &str) {
    {
        let mut sched = state.scheduler.lock().expect("scheduler 锁");
        if sched.device_state(uuid) != Some(DeviceState::Offline) {
            return; // 无需恢复
        }
        if let Err(e) = sched.device_back(uuid) {
            warn!("device_back({uuid}) 失败: {e}");
            return;
        }
    }
    debug!("设备 {uuid} 恢复上线");
    state.audit.record(
        AuditEvent::DeviceBack,
        "system",
        0,
        None,
        &[uuid.to_string()],
        "设备故障恢复上线",
    );
    state.broadcast_devices();
    jobs::pump(state);
    crate::recovery::persist(state);
}