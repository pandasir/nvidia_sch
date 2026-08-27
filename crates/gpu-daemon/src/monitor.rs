//! 监控轮询任务:周期调用 provider 的枚举(设备清单)与轮询(指标),
//! 更新共享监控快照并广播设备状态变更。
//!
//! M4 起承担**设备故障检测**(DESIGN §9):
//!
//! - 每轮做设备清单对账 [`reconcile`]:新设备热插拔注册 / 设备消失
//!   (热插拔、驱动重载)经 **FAULT_DEBOUNCE 个连续周期**确认后下线 /
//!   消失的设备重新出现则恢复上线;
//! - NVML 调用是同步 FFI,统一在 `spawn_blocking` 中执行。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use nvmlx::{DeviceProvider, DeviceSnapshot};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::faults;
use crate::state::AppState;

/// 设备消失连续多少个周期才触发下线(防抖,DESIGN §9:
/// "故障判定连续 N 个周期才触发下线")
pub const FAULT_DEBOUNCE: u32 = 2;

/// 设备对账的跨轮状态(每轮更新)
#[derive(Debug, Default)]
pub struct ReconcileState {
    /// 上一轮见过的设备集合
    seen: HashSet<String>,
    /// uuid -> 已连续缺失的轮数
    missing: HashMap<String, u32>,
}

/// 单轮设备对账:
///
/// - 当前存在的设备:刷新静态信息;若处于 `Offline` → 恢复上线
///   ([`faults::handle_device_back`]),并从缺失计数清除;
/// - 曾见过、本轮缺失的设备:缺失计数 +1,连续 [`FAULT_DEBOUNCE`] 轮
///   缺失 → [`faults::handle_device_offline`](撤销租约、杀进程、置 Offline);
/// - 新设备(热插拔):注册进 scheduler 与 monitor(初始 Free)。
///
/// 跨轮状态 [`ReconcileState`] 保留:未确认的缺失设备会在下轮继续计数。
pub fn reconcile(state: &Arc<AppState>, current: &[DeviceSnapshot], rec: &mut ReconcileState) {
    let current_set: HashSet<String> = current.iter().map(|d| d.uuid.clone()).collect();

    // ① 当前存在的设备:刷新 + 恢复
    for d in current {
        let was_offline = {
            let sched = state.scheduler.lock().expect("scheduler 锁");
            sched.device_state(&d.uuid) == Some(gpu_core::model::DeviceState::Offline)
        };
        upsert_monitor_and_scheduler(state, d);
        rec.missing.remove(&d.uuid);
        if was_offline {
            debug!("设备 {} 重新出现,恢复上线", d.uuid);
            faults::handle_device_back(state, &d.uuid);
        }
    }

    // ② 消失检测:能参与跟踪的 = 已见过(seen)+ 上一轮起就在缺失(missing)
    let track: HashSet<String> = rec
        .seen
        .iter()
        .chain(rec.missing.keys())
        .cloned()
        .collect();
    let mut confirmed: Vec<String> = Vec::new();
    for u in &track {
        if !current_set.contains(u) {
            let n = rec.missing.entry(u.clone()).or_insert(0);
            *n += 1;
            debug!("设备 {u} 缺失(第 {n}/{FAULT_DEBOUNCE} 轮)");
            if *n >= FAULT_DEBOUNCE {
                confirmed.push(u.clone());
                rec.missing.remove(u);
            }
        }
    }
    for u in &confirmed {
        rec.seen.remove(u);
        faults::handle_device_offline(state, u);
    }

    // ③ seen = 本轮集合 + 未确认的缺失设备(下轮继续计数)
    let mut seen: HashSet<String> = current_set;
    for u in rec.missing.keys() {
        seen.insert(u.clone());
    }
    rec.seen = seen;
}

/// 注册/刷新单个设备:monitor 快照 + scheduler(upsert 保留 Offline/Allocated)
fn upsert_monitor_and_scheduler(state: &AppState, d: &DeviceSnapshot) {
    {
        let mut mon = state.monitor.write().expect("monitor 锁");
        match mon.get_mut(&d.uuid) {
            Some(entry) => entry.info = d.clone(),
            None => {
                mon.insert(
                    d.uuid.clone(),
                    crate::state::MonitorEntry {
                        info: d.clone(),
                        metrics: None,
                    },
                );
            }
        }
    }
    {
        let mut sched = state.scheduler.lock().expect("scheduler 锁");
        sched.upsert_device(gpu_core::model::DeviceInfo {
            uuid: d.uuid.clone(),
            name: d.name.clone(),
            pci_bus_id: d.pci_bus_id.clone(),
            memory_total_mib: d.memory_total_mib,
            state: gpu_core::model::DeviceState::Free, // upsert 对已存在设备只刷新静态信息
        });
    }
}

/// 启动监控轮询任务。`shutdown_rx` 翻转为 true 时优雅退出。
pub fn spawn_monitor(
    state: Arc<AppState>,
    provider: Arc<dyn DeviceProvider>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rec = ReconcileState::default();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // M4:设备清单对账(NVML 枚举亦为同步 FFI)
                    let provider_a = provider.clone();
                    let listed = tokio::task::spawn_blocking(move || provider_a.list_devices()).await;
                    match listed {
                        Ok(Ok(list)) => reconcile(&state, &list, &mut rec),
                        Ok(Err(e)) => warn!("设备枚举失败: {e}"),
                        Err(e) => warn!("枚举任务执行失败: {e}"),
                    }

                    // 指标轮询
                    let provider_b = provider.clone();
                    let polled = tokio::task::spawn_blocking(move || provider_b.poll()).await;
                    match polled {
                        Ok(Ok(metrics)) => {
                            let mut mon = state.monitor.write().expect("monitor 锁");
                            for m in &metrics {
                                if let Some(entry) = mon.get_mut(&m.uuid) {
                                    entry.metrics = Some(m.clone());
                                } else {
                                    debug!("指标出现未知设备 uuid={}", m.uuid);
                                }
                            }
                            drop(mon);
                            state.broadcast_devices();
                        }
                        Ok(Err(e)) => warn!("指标轮询失败: {e}"),
                        Err(e) => warn!("轮询任务执行失败: {e}"),
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        debug!("监控轮询收到退出信号");
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpu_core::model::JobStatus;
    use gpu_core::scheduler::{Scheduler, SubmitRequest};
    use gpu_core::SchedulerConfig;
    use std::time::Duration;

    fn snap(uuid: &str) -> DeviceSnapshot {
        DeviceSnapshot {
            uuid: uuid.into(),
            name: "T".into(),
            pci_bus_id: "0000:00:00.0".into(),
            memory_total_mib: 16384,
            unix_minor: None,
        }
    }

    fn test_state(devices: Vec<&str>) -> Arc<AppState> {
        let mut sched = Scheduler::new(SchedulerConfig {
            queue_timeout_secs: 60,
            ..Default::default()
        });
        for u in &devices {
            sched.upsert_device(gpu_core::model::DeviceInfo {
                uuid: u.to_string(),
                name: "T".into(),
                pci_bus_id: "".into(),
                memory_total_mib: 1,
                state: gpu_core::model::DeviceState::Free,
            });
        }
        AppState::new(
            sched,
            devices.iter().map(|u| snap(u)).collect(),
            Arc::new(crate::audit::AuditLog::new(None).unwrap()),
            Arc::new(crate::cgroup::NoopCgroup),
            None,
        )
    }

    /// 提交一个最简单的作业(立即 Allocated,无进程 —— 设备消失后应 Failed)
    fn submit(state: &Arc<AppState>) -> String {
        let mut sched = state.scheduler.lock().unwrap();
        let (id, _) = sched
            .submit(SubmitRequest {
                owner: "tester".into(),
                description: "".into(),
                gpus_requested: 1,
                priority: 5,
                queue_timeout_secs: None,
            })
            .unwrap();
        drop(sched);
        id
    }

    #[tokio::test]
    async fn device_missing_debounces_then_offline() {
        let state = test_state(vec!["gpu-0"]);
        let mut rec = ReconcileState::default();

        // 第 0 轮:清单中存在
        reconcile(&state, &[snap("gpu-0")], &mut rec);
        assert_eq!(
            state.scheduler.lock().unwrap().device_state("gpu-0"),
            Some(gpu_core::model::DeviceState::Free)
        );

        // 第 1 轮缺失:未达阈值,仍 Free
        reconcile(&state, &[], &mut rec);
        assert_eq!(
            state.scheduler.lock().unwrap().device_state("gpu-0"),
            Some(gpu_core::model::DeviceState::Free)
        );

        // 第 2 轮缺失:确认下线
        reconcile(&state, &[], &mut rec);
        assert_eq!(
            state.scheduler.lock().unwrap().device_state("gpu-0"),
            Some(gpu_core::model::DeviceState::Offline)
        );
    }

    #[tokio::test]
    async fn device_offline_fails_running_job_and_frees() {
        let state = test_state(vec!["gpu-0"]);
        let job_id = submit(&state);
        // 模拟 spawn 成功后(try_allocate 已分配、lease 已建)
        assert_eq!(
            state.scheduler.lock().unwrap().job(&job_id).unwrap().status,
            JobStatus::Allocated
        );

        let mut rec = ReconcileState::default();
        reconcile(&state, &[snap("gpu-0")], &mut rec);
        reconcile(&state, &[], &mut rec);
        reconcile(&state, &[], &mut rec); // 2 轮缺失 → 下线

        let sched = state.scheduler.lock().unwrap();
        let job = sched.job(&job_id).unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(sched.lease(&job_id), None, "租约应已被撤销");
        assert_eq!(sched.device_state("gpu-0"), Some(gpu_core::model::DeviceState::Offline));
        assert_eq!(sched.free_count(), 0, "故障卡不参与分配");
    }

    #[tokio::test]
    async fn device_back_restores_free() {
        let state = test_state(vec!["gpu-0"]);
        let mut rec = ReconcileState::default();
        reconcile(&state, &[snap("gpu-0")], &mut rec);
        reconcile(&state, &[], &mut rec);
        reconcile(&state, &[], &mut rec); // → Offline
        // 设备重新出现 → 恢复 Free
        reconcile(&state, &[snap("gpu-0")], &mut rec);
        // 轮到下一轮?恢复检测需要 missing 里有记录:上文已 remove;重新 seen 有 gpu-0
        // 直接断言:恢复后应 Free
        std::thread::sleep(Duration::from_millis(5));
        // reconcile 内 handle_device_back 同步完成
        reconcile(&state, &[snap("gpu-0")], &mut rec); // 再跑一轮确保稳定
        assert_eq!(
            state.scheduler.lock().unwrap().device_state("gpu-0"),
            Some(gpu_core::model::DeviceState::Free)
        );
    }

    #[test]
    fn hotplug_new_device_registered() {
        let state = test_state(vec![]);
        let mut rec = ReconcileState::default();
        reconcile(&state, &[snap("gpu-9")], &mut rec);
        assert_eq!(
            state.scheduler.lock().unwrap().device_state("gpu-9"),
            Some(gpu_core::model::DeviceState::Free)
        );
    }
}