//! M4:Xid 关键事件监听(仅 Linux)。
//!
//! NVIDIA 驱动在 GPU 致命故障(掉卡、显存损坏、过热保护等)时抛出 Xid 码,
//! 是设备故障的最快信号(DESIGN §9 故障信号之一)。
//!
//! - 经 nvmlx 的 `NvmlProvider::register_xid_events()` 注册全卡
//!   `CRITICAL_XID_ERROR` 事件集(依赖 nvml-wrapper,其 API 为
//!   `#[cfg(target_os = "linux")]`,macOS 上本模块整体不编译);
//! - 收到 Xid → 与 monitor 的设备消失检测共用同一下线入口
//!   [`crate::faults::handle_device_offline`](幂等,双信号并存不冲突);
//! - 无卡环境/NVML 不可用/非 NVML 后端 → 自动不激活,不影响其他链路。

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::faults;
use crate::state::AppState;

/// 阻塞轮询粒度(每次 wait 的超时)
const XID_WAIT_MS: u32 = 1000;

/// 启动 Xid 监听任务。仅对 NVML 后端生效(其他后端 downcast 失败即静默退出)。
pub fn spawn_xid_watcher(
    state: Arc<AppState>,
    provider: Arc<dyn nvmlx::DeviceProvider>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // 仅 NVML 后端具备事件能力(NullProvider 等回落静默)
        let nvml = match provider.as_any().downcast_ref::<nvmlx::NvmlProvider>() {
            Some(n) => n,
            None => {
                debug!("非 NVML 后端,不启用 Xid 事件监听");
                return;
            }
        };
        let mut watcher = match nvml.register_xid_events() {
            Ok(w) => w,
            Err(e) => {
                warn!("Xid 事件注册失败(无卡或驱动不支持): {e}");
                return;
            }
        };
        if watcher.device_count() == 0 {
            debug!("无 GPU 可监听 Xid 事件");
            return;
        }
        info!(
            "Xid 事件监听已启用({} 张卡);关键 GPU 错误将触发设备下线",
            watcher.device_count()
        );

        loop {
            match watcher.wait_one(XID_WAIT_MS) {
                Ok(Some(n)) => {
                    // Xid 事件本身即故障证据,无需防抖;与 monitor 的
                    // 清单对账(防抖 2 轮)互为兜底:
                    // - Xid 先行:立即下线(最快路径,卡可能仍在树中);
                    // - 掉卡消失:清单对账兜底(即使错过 Xid 也能下线)。
                    warn!(
                        "检测到关键 GPU 错误 Xid={} (uuid={}) → 触发设备下线",
                        n.xid, n.uuid
                    );
                    faults::handle_device_offline(&state, &n.uuid);
                }
                Ok(None) => {}
                Err(e) => {
                    // GpuLost 等:卡已掉总线,交给 monitor 清单对账;
                    // 短暂退避后重试,避免热插拔后丢失新卡的事件注册。
                    warn!("Xid 监听异常: {e}(5s 后重试)");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    })
}