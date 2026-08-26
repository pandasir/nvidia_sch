//! 监控轮询任务:周期调用 provider.poll()(NVML 同步 FFI,放 spawn_blocking),
//! 更新共享监控快照并广播设备状态变更。

use std::sync::Arc;
use std::time::Duration;

use nvmlx::DeviceProvider;
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::state::AppState;

/// 启动监控轮询任务。`shutdown_rx` 翻转为 true 时优雅退出。
pub fn spawn_monitor(
    state: Arc<AppState>,
    provider: Arc<dyn DeviceProvider>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // NVML 调用是同步 FFI,阻塞型后端应在线程池中执行
                    let provider = provider.clone();
                    let polled = tokio::task::spawn_blocking(move || provider.poll()).await;
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
                            broadcast_status(&state);
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

/// 广播当前设备状态快照(逐设备发送,WatchDevices 逐条推送)
fn broadcast_status(state: &AppState) {
    for status in state.status_list() {
        if state.tx.send(status).is_err() {
            // 无订阅者,忽略
            break;
        }
    }
}
