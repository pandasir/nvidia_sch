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
}
