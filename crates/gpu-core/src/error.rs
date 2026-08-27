//! 调度核心的错误类型

use crate::model::JobStatus;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchedulerError {
    #[error("job {job_id} 不存在")]
    JobNotFound { job_id: String },

    #[error("job {job_id} 当前状态 {status:?} 不允许该操作")]
    InvalidJobState { job_id: String, status: JobStatus },

    #[error("设备 {uuid} 不存在")]
    DeviceNotFound { uuid: String },

    #[error("请求不合法: {0}")]
    InvalidRequest(String),

    /// M5:提交配额(并发作业数)被拒;运行期配额(在飞卡数)则入队等待,不报错
    #[error("用户 {owner} 超出配额: {detail}")]
    QuotaExceeded { owner: String, detail: String },
}
