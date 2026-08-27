//! 设备发现抽象:调度核心与监控层只依赖本 trait,不直接接触 NVML。

/// 设备静态信息:从任意后端(NVML / mock)获取的设备基本信息
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceSnapshot {
    pub uuid: String,
    pub name: String,
    pub pci_bus_id: String,
    pub memory_total_mib: u64,
    /// Linux 设备文件次设备号(即 /dev/nvidia<N> 的 N,真机自 NVML
    /// `minor_number` 获取);用于 cgroup 设备过滤白名单(M3)。
    /// 无卡后端(mock)为 None。
    pub unix_minor: Option<u32>,
}

/// 设备实时指标(监控轮询输出)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceMetrics {
    pub uuid: String,
    /// GPU 利用率百分比 (0-100)
    pub utilization_percent: u32,
    /// 已用显存 (MiB)
    pub used_memory_mib: u64,
    /// 温度 (摄氏度)
    pub temperature_c: u32,
}

/// 设备发现后端错误
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("NVML 初始化失败: {0}")]
    Init(String),
    #[error("设备枚举失败: {0}")]
    Enumeration(String),
    #[error("指标轮询失败: {0}")]
    Poll(String),
    #[error("provider 不可用: {0}")]
    Unavailable(String),
    #[error("io 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("xid 事件监听失败: {0}")]
    Xid(String),
}

/// 设备发现与监控后端
pub trait DeviceProvider: Send + Sync {
    /// 枚举当前可见的 GPU(静态信息)
    fn list_devices(&self) -> Result<Vec<DeviceSnapshot>, ProviderError>;

    /// 轮询实时指标。默认实现返回空,无指标的静态后端无需实现
    fn poll(&self) -> Result<Vec<DeviceMetrics>, ProviderError> {
        Ok(Vec::new())
    }

    /// 类型擦除访问(M4:daemon 需要 downcast 到具体 provider
    /// 以启用后端特有能力,如 Xid 事件监听)
    fn as_any(&self) -> &dyn std::any::Any;
}
