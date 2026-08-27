//! 领域模型:设备、作业、租约、状态枚举。
//!
//! 字段刻意全部使用 serde 友好的基础类型(String/u64/枚举),
//! 便于快照持久化与审计事件落盘。

use serde::{Deserialize, Serialize};

/// 设备状态机:Free → Allocated → (Offline)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceState {
    /// 空闲,可分配
    Free,
    /// 已被某租约占用
    Allocated,
    /// 故障/维护,不参与分配
    Offline,
}

/// 物理 GPU 在调度视角的信息
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// 设备唯一标识。**一律用 UUID,不用索引**(插槽/拓扑变化会导致索引漂移)
    pub uuid: String,
    /// 型号名
    pub name: String,
    /// PCI bus id(如 0000:01:00.0)
    pub pci_bus_id: String,
    /// 显存总量 (MiB)
    pub memory_total_mib: u64,
    /// 当前状态
    pub state: DeviceState,
}

/// 作业生命周期:Submitted → Queued → Allocated → Running → 终态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    /// 已受理(瞬时状态,随后立即入队)
    Submitted,
    /// 排队等待资源
    Queued,
    /// 已分配资源,进程尚未拉起(或拉起中)
    Allocated,
    /// 进程已在运行
    Running,
    /// 成功结束
    Succeeded,
    /// 失败(细节见 [`FailureReason`])
    Failed,
}

impl JobStatus {
    /// 是否终态(此后不再参与调度,设备已释放)
    pub fn is_terminal(&self) -> bool {
        matches!(self, JobStatus::Succeeded | JobStatus::Failed)
    }
}

/// 失败原因(终态为 `Failed` 时的细节)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureReason {
    /// 排队中被用户取消
    Cancelled,
    /// 排队超时(max_queue_timeout,默认 1h,ADR-2)
    QueueTimeout,
    /// 被管理员/owner Kill
    Killed,
    /// 租约过期且未续期
    LeaseExpired,
    /// 所在设备故障下线
    DeviceOffline,
    /// 进程已退出但退出码非零(含 exec 失败,如命令不存在)
    ExitedNonzero,
    /// 其他原因
    Other(String),
}

/// 一个提交的任务(带资源需求)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// 归属用户/身份:审计与配额记账用
    pub owner: String,
    /// 展示用描述
    pub description: String,
    /// 请求的 GPU 数量(整卡粒度,v1)
    pub gpus_requested: u32,
    /// 优先级,越大越优先(v1 固定值,不支持运行中调整)
    pub priority: u8,
    /// 提交序号:调度器内单调递增,同优先级下严格先到先得(FIFO)
    pub seq: u64,
    pub status: JobStatus,
    pub failure: Option<FailureReason>,
    /// 提交时间 (epoch ms)
    pub submitted_at_ms: u64,
    /// 排队超时秒数(0 = 不限制)
    pub queue_timeout_secs: u64,
}

/// 租约:设备在某段时间内归属某作业的授权凭证
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub job_id: String,
    /// 占用的物理卡 UUID 列表
    pub device_uuids: Vec<String>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub heartbeat_at_ms: u64,
}
