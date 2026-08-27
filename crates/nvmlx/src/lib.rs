//! nvmlx —— GPU 设备访问抽象层。
//!
//! - [`DeviceProvider`] trait:调度核心与监控层只依赖它,不直接接触 NVML;
//! - [`NullProvider`]:无硬件开发后端(macOS 开发机全链路可用);
//! - `NvmlProvider`(feature `nvml`,默认开启):基于 nvml-wrapper 的真机实现,
//!   运行时动态加载驱动库,无驱动环境下初始化返回 [`ProviderError::Init`]。

mod null;
#[cfg(feature = "nvml")]
mod nvml;
pub mod traits;

pub use null::NullProvider;
#[cfg(feature = "nvml")]
pub use nvml::NvmlProvider;
#[cfg(all(feature = "nvml", target_os = "linux"))]
pub use nvml::{XidNotification, XidWatcher};
pub use traits::{DeviceMetrics, DeviceProvider, DeviceSnapshot, ProviderError};
