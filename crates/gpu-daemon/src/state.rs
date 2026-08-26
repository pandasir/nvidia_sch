//! 全局共享状态:调度核心 + 监控快照 + 设备状态广播。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use gpu_api::grpc;
use gpu_core::scheduler::Scheduler;
use nvmlx::{DeviceMetrics, DeviceSnapshot};
use tokio::sync::broadcast;

/// 单个设备当前可见状态 = 静态信息 + 最新指标
#[derive(Debug, Clone)]
pub struct MonitorEntry {
    pub info: DeviceSnapshot,
    pub metrics: Option<DeviceMetrics>,
}

/// 全局共享状态(daemon 单进程内共享)
pub struct AppState {
    /// 调度核心(单写者:gRPC 层持有互斥)
    pub scheduler: Mutex<Scheduler>,
    /// 最新监控快照:uuid -> 条目
    pub monitor: RwLock<HashMap<String, MonitorEntry>>,
    /// 设备状态变更广播(gRPC WatchDevices 的推送通道)
    pub tx: broadcast::Sender<grpc::DeviceStatus>,
}

impl AppState {
    pub fn new(scheduler: Scheduler, discovered: Vec<DeviceSnapshot>) -> Arc<Self> {
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
        })
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
