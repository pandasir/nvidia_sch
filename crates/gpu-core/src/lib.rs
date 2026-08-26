//! gpu-core —— 调度核心的领域模型与纯状态机。
//!
//! 本 crate 是唯一承载业务逻辑的 crate,遵循两条铁律:
//!
//! - **零 I/O、零副作用**:不直接调用 NVML(设备信息经 `nvmlx` 的
//!   `DeviceProvider` trait 注入)、不发信号、不启进程、不写磁盘;
//! - **时间由调用方注入**:所有与时间相关的推进都通过显式参数
//!   (如 [`scheduler::Scheduler::tick`] 的 `now_ms`)驱动。
//!
//! 因此调度逻辑可以在没有真实 GPU 的环境下做全量单元测试,也可以在未来
//! 平滑升级为多机调度(状态机保持纯逻辑,共识层替换内存态即可)。

pub mod error;
pub mod model;
pub mod scheduler;

pub use error::SchedulerError;
pub use model::{DeviceInfo, DeviceState, FailureReason, Job, JobStatus, Lease};
pub use scheduler::{Scheduler, SchedulerConfig, SchedulerEvent, SchedulerSnapshot, SubmitRequest};
