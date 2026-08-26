//! NvmlProvider:基于 nvml-wrapper 的真机实现。
//!
//! NVML 库在运行时动态加载,无 NVIDIA 驱动的机器上 `new()` 返回
//! [`ProviderError::Init`] 而非编译失败 —— 因此本模块可跨平台编译,
//! 在无卡开发机(macOS)上依然能完成类型检查与集成测试。

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

use crate::{DeviceMetrics, DeviceProvider, DeviceSnapshot, ProviderError};

/// 真机 NVML 后端
pub struct NvmlProvider {
    nvml: Nvml,
}

fn mi_b(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

impl NvmlProvider {
    pub fn new() -> Result<Self, ProviderError> {
        match Nvml::init() {
            Ok(nvml) => Ok(Self { nvml }),
            Err(e) => Err(ProviderError::Init(e.to_string())),
        }
    }

    fn enumerate(&self) -> Result<Vec<DeviceSnapshot>, ProviderError> {
        let count = self
            .nvml
            .device_count()
            .map_err(|e| ProviderError::Enumeration(e.to_string()))?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let dev = self
                .nvml
                .device_by_index(i)
                .map_err(|e| ProviderError::Enumeration(format!("index {i}: {e}")))?;
            let uuid = dev
                .uuid()
                .map_err(|e| ProviderError::Enumeration(format!("uuid[{i}]: {e}")))?;
            let name = dev
                .name()
                .map_err(|e| ProviderError::Enumeration(format!("name[{i}]: {e}")))?;
            let pci = dev
                .pci_info()
                .map_err(|e| ProviderError::Enumeration(format!("pci[{i}]: {e}")))?;
            let mem = dev
                .memory_info()
                .map_err(|e| ProviderError::Enumeration(format!("memory[{i}]: {e}")))?;
            out.push(DeviceSnapshot {
                uuid,
                name,
                pci_bus_id: pci.bus_id,
                memory_total_mib: mi_b(mem.total),
            });
        }
        Ok(out)
    }
}

impl DeviceProvider for NvmlProvider {
    fn list_devices(&self) -> Result<Vec<DeviceSnapshot>, ProviderError> {
        self.enumerate()
    }

    fn poll(&self) -> Result<Vec<DeviceMetrics>, ProviderError> {
        let count = self
            .nvml
            .device_count()
            .map_err(|e| ProviderError::Poll(e.to_string()))?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let dev = self
                .nvml
                .device_by_index(i)
                .map_err(|e| ProviderError::Poll(format!("index {i}: {e}")))?;
            let uuid = dev
                .uuid()
                .map_err(|e| ProviderError::Poll(format!("uuid[{i}]: {e}")))?;
            let util = dev
                .utilization_rates()
                .map_err(|e| ProviderError::Poll(format!("utilization[{i}]: {e}")))?;
            let mem = dev
                .memory_info()
                .map_err(|e| ProviderError::Poll(format!("memory[{i}]: {e}")))?;
            let temp = dev
                .temperature(TemperatureSensor::Gpu)
                .map_err(|e| ProviderError::Poll(format!("temperature[{i}]: {e}")))?;
            out.push(DeviceMetrics {
                uuid,
                utilization_percent: util.gpu,
                used_memory_mib: mi_b(mem.used),
                temperature_c: temp,
            });
        }
        Ok(out)
    }
}
