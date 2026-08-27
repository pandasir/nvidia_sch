//! NvmlProvider:基于 nvml-wrapper 的真机实现。
//!
//! NVML 库在运行时动态加载,无 NVIDIA 驱动的机器上 `new()` 返回
//! [`ProviderError::Init`] 而非编译失败 —— 因此本模块可跨平台编译,
//! 在无卡开发机(macOS)上依然能完成类型检查与集成测试。

use std::sync::Arc;

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

use crate::{DeviceMetrics, DeviceProvider, DeviceSnapshot, ProviderError};

/// 真机 NVML 后端
pub struct NvmlProvider {
    /// Arc 共享:M4 的 Xid 事件监听需要与 provider 共享同一 NVML 实例
    nvml: Arc<Nvml>,
}

fn mi_b(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

impl NvmlProvider {
    pub fn new() -> Result<Self, ProviderError> {
        match Nvml::init() {
            Ok(nvml) => Ok(Self {
                nvml: Arc::new(nvml),
            }),
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
                // 设备文件次设备号:仅 Linux 存在该 NVML 接口;驱动未加载/
                // 无卡时可能失败,失败则 None(cgroup 白名单构建时自动跳过并告警)
                #[cfg(target_os = "linux")]
                unix_minor: dev.minor_number().ok(),
                #[cfg(not(target_os = "linux"))]
                unix_minor: None,
            });
        }
        Ok(out)
    }
}

impl DeviceProvider for NvmlProvider {
    fn list_devices(&self) -> Result<Vec<DeviceSnapshot>, ProviderError> {
        self.enumerate()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
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

// ---------------------------------------------------------------------------
// M4:Xid 事件监听(Linux only —— nvml-wrapper 的 register_events 为
// `#[cfg(target_os = "linux")]`,macOS 上完全不参与编译)。
//
// CRITICAL_XID_ERROR 是"致命 GPU 错误"事件(NVIDIA 驱动抛出 Xid 码,
// 如 43 = 掉卡、48 = 显存故障等),收到即意味着设备已不可用/即将不可用,
// 应触发设备下线流程(DESIGN §9 故障信号之一)。
// ---------------------------------------------------------------------------

/// Xid 事件通知
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct XidNotification {
    pub uuid: String,
    pub xid: u64,
}

/// 单卡的 EventSet 包装(生命周期由 [`XidWatcher`] 的 `_nvml` 字段保活)
#[cfg(target_os = "linux")]
struct XidEventSet {
    set: nvml_wrapper::event::EventSet<'static>,
}

/// 自包含的 Xid 监听器:持有共享 NVML 实例与全卡的 EventSet
#[cfg(target_os = "linux")]
pub struct XidWatcher {
    /// 保活底层的 `Nvml`;EventSet 内部引用它,故需与 provider 共享同一 Arc
    _nvml: Arc<Nvml>,
    sets: Vec<(String, XidEventSet)>,
}

#[cfg(target_os = "linux")]
impl XidWatcher {
    /// 阻塞等待任一卡的事件(至多 `timeout_ms`)。无事件返回 `Ok(None)`。
    /// 收到 Xid 错误返回 [`XidNotification`];`GpuLost` 视为监听失效
    /// (卡已掉总线,由 monitor 的清单对账接管下线)。
    pub fn wait_one(
        &mut self,
        timeout_ms: u32,
    ) -> Result<Option<XidNotification>, ProviderError> {
        use nvml_wrapper::error::NvmlError;
        use nvml_wrapper::enums::event::XidError as WrapperXid;

        for (key, es) in self.sets.iter_mut() {
            match es.set.wait(timeout_ms) {
                Ok(data) => {
                    // 事件属于哪张卡以事件数据为准(热插拔后 UUID 可能变化)
                    let uuid = data
                        .device
                        .uuid()
                        .map_err(|e| ProviderError::Xid(format!("uuid: {e}")))?;
                    let xid = match data.event_data {
                        Some(WrapperXid::Value(v)) => v,
                        _ => u64::MAX, // Unknown / 非 Xid 事件类型兜底
                    };
                    let _ = key;
                    return Ok(Some(XidNotification { uuid, xid }));
                }
                Err(NvmlError::Timeout) => continue,
                Err(NvmlError::GpuLost) => {
                    let _ = key;
                    return Err(ProviderError::Xid(
                        "GPU 已掉总线(GpuLost),由清单对账接管".into(),
                    ));
                }
                Err(e) => {
                    let _ = key;
                    return Err(ProviderError::Xid(format!("wait: {e}")));
                }
            }
        }
        Ok(None)
    }

    /// 卡数(空 = 无卡可监听)
    pub fn device_count(&self) -> usize {
        self.sets.len()
    }
}

impl NvmlProvider {
    /// Linux:为所有当前可见的 GPU 注册 CRITICAL_XID_ERROR 事件集。
    ///
    /// 单卡注册失败(部分型号/驱动不支持)会跳过并在日志说明,不阻断整体。
    /// 注意:事件集与 provider 共享同一 NVML 实例,调用方存活期间
    /// 必须同时持有 [`XidWatcher`] —— 结构上已通过内部 `Arc` 保证。
    #[cfg(target_os = "linux")]
    pub fn register_xid_events(&self) -> Result<XidWatcher, ProviderError> {
        use nvml_wrapper::bitmasks::event::EventTypes;

        let count = self
            .nvml
            .device_count()
            .map_err(|e| ProviderError::Xid(format!("device_count: {e}")))?;
        let mut sets = Vec::new();
        for i in 0..count {
            let dev = self
                .nvml
                .device_by_index(i)
                .map_err(|e| ProviderError::Enumeration(format!("index {i}: {e}")))?;
            let uuid = dev
                .uuid()
                .map_err(|e| ProviderError::Enumeration(format!("uuid[{i}]: {e}")))?;
            let set = self
                .nvml
                .create_event_set()
                .map_err(|e| ProviderError::Xid(format!("create_event_set[{i}]: {e}")))?;
            let set = match dev.register_events(EventTypes::CRITICAL_XID_ERROR, set) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("设备 {uuid} 不支持 Xid 事件注册,跳过: {}", e.error);
                    continue;
                }
            };
            // 生命周期安全论证:EventSet 内部仅借用 NVML 的符号表;
            // `_nvml: Arc<Nvml>` 与本结构同存亡,保证底层引用始终有效。
            let set = unsafe {
                std::mem::transmute::<nvml_wrapper::event::EventSet<'_>, nvml_wrapper::event::EventSet<'static>>(set)
            };
            sets.push((uuid, XidEventSet { set }));
        }
        Ok(XidWatcher {
            _nvml: self.nvml.clone(),
            sets,
        })
    }
}
