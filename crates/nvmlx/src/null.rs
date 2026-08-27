//! NullProvider:无硬件后端的开发/测试实现。
//!
//! M0 起 daemon 默认使用它,使调度核心可在无 GPU 的机器(macOS 开发机)
//! 上完整跑通;M1 起 `poll()` 输出确定性模拟指标,可用于演示 `gpu ls`
//! 与 /metrics 的实时刷新效果。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{DeviceMetrics, DeviceProvider, DeviceSnapshot, ProviderError};

/// 返回固定设备列表(默认空),轮询产出模拟指标。
#[derive(Debug, Default)]
pub struct NullProvider {
    devices: Vec<DeviceSnapshot>,
    /// 轮询次数,驱动模拟指标变化
    tick: AtomicU64,
}

impl NullProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用一组虚拟设备构造,便于开发联调与端到端演练
    pub fn with_devices(devices: Vec<DeviceSnapshot>) -> Self {
        Self {
            devices,
            tick: AtomicU64::new(0),
        }
    }
}

impl DeviceProvider for NullProvider {
    fn list_devices(&self) -> Result<Vec<DeviceSnapshot>, ProviderError> {
        Ok(self.devices.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn poll(&self) -> Result<Vec<DeviceMetrics>, ProviderError> {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .devices
            .iter()
            .map(|d| {
                // 由 uuid 与轮询次数决定的确定性伪随机指标(演示用)
                let mut h = DefaultHasher::new();
                d.uuid.hash(&mut h);
                let seed = h.finish();

                let utilization = ((tick * 13 + seed % 97) % 101) as u32; // 0..=100
                let used = (tick * 7 + seed % 1000) % (d.memory_total_mib / 2 + 1);
                let temperature = 40 + ((tick * 3 + seed % 29) % 31) as u32; // 40..=70

                DeviceMetrics {
                    uuid: d.uuid.clone(),
                    utilization_percent: utilization,
                    used_memory_mib: used,
                    temperature_c: temperature,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(uuid: &str) -> DeviceSnapshot {
        DeviceSnapshot {
            uuid: uuid.into(),
            name: "Fake GPU".into(),
            pci_bus_id: "0000:00:00.0".into(),
            memory_total_mib: 16384,
            unix_minor: None,
        }
    }

    #[test]
    fn null_provider_returns_configured_devices() {
        let p = NullProvider::with_devices(vec![fake("gpu-fake-1")]);
        let devices = p.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].uuid, "gpu-fake-1");
    }

    #[test]
    fn default_null_provider_is_empty() {
        let p = NullProvider::new();
        assert!(p.list_devices().unwrap().is_empty());
    }

    #[test]
    fn poll_produces_metrics_within_bounds() {
        let p = NullProvider::with_devices(vec![fake("gpu-0"), fake("gpu-1")]);
        let m = p.poll().unwrap();
        assert_eq!(m.len(), 2);
        for e in &m {
            assert!(e.utilization_percent <= 100);
            assert!(e.temperature_c >= 40 && e.temperature_c <= 70);
            assert!(e.used_memory_mib <= 8192);
        }
    }

    #[test]
    fn poll_metrics_change_over_ticks() {
        let p = NullProvider::with_devices(vec![fake("gpu-0")]);
        let first = p.poll().unwrap();
        let second = p.poll().unwrap();
        assert_ne!(first, second, "模拟指标应在轮询间变化");
    }
}
