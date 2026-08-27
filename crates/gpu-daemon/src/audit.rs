//! 审计日志(M3):append-only JSON Lines 事件流,是"事实来源"(DESIGN.md §10)。
//!
//! 每行一个事件:`{ts, event, user, uid, job_id, device_uuids, detail}`,
//! 覆盖 分配/释放/失败/Kill/取消 等全部关键动作,用于审计、记账与复盘。
//! 写入失败仅告警、不阻断主流程(审计是记账,不是执行路径)。

use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 审计日志:所有记录追加写入单一 JSONL 文件(跨进程 append 安全)
pub struct AuditLog {
    file: Mutex<Option<File>>,
}

impl AuditLog {
    /// `path = None` 表示关闭审计(开发/测试默认);`Some(p)` 时创建/追加打开
    pub fn new(path: Option<PathBuf>) -> std::io::Result<Self> {
        let file = match path {
            Some(p) => {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Some(OpenOptions::new().create(true).append(true).open(&p)?)
            }
            None => None,
        };
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// 追加一条审计事件。失败仅记录 tracing 告警。
    pub fn log(&self, rec: AuditRecord) {
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!("审计锁 poisoned,丢弃事件: {}", rec.event);
                return;
            }
        };
        let Some(file) = guard.as_mut() else { return };
        let line = match serde_json::to_string(&rec) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("审计序列化失败: {e}");
                return;
            }
        };
        if let Err(e) = writeln!(file, "{line}").and_then(|_| file.flush()) {
            tracing::warn!("审计写入失败: {e}");
        }
    }

    /// 便捷入口:构造并写入一条记录
    pub fn record(
        &self,
        event: AuditEvent,
        user: &str,
        uid: u32,
        job_id: Option<&str>,
        devices: &[String],
        detail: impl Into<String>,
    ) {
        self.log(AuditRecord {
            ts: now_ms(),
            event: event.to_str().into(),
            user: user.into(),
            uid,
            job_id: job_id.map(String::from),
            device_uuids: devices.to_vec(),
            detail: detail.into(),
        });
    }
}

/// 审计事件类别(DESIGN.md §10 的 event 维度)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEvent {
    SubmitJob,
    JobQueued,
    JobAllocated,
    JobRunning,
    JobSucceeded,
    JobFailed,
    KillJob,
    CancelJob,
    DeviceOffline,
    DeviceBack,
    LeaseExpired,
}

impl AuditEvent {
    fn to_str(self) -> &'static str {
        match self {
            AuditEvent::SubmitJob => "submit_job",
            AuditEvent::JobQueued => "job_queued",
            AuditEvent::JobAllocated => "job_allocated",
            AuditEvent::JobRunning => "job_running",
            AuditEvent::JobSucceeded => "job_succeeded",
            AuditEvent::JobFailed => "job_failed",
            AuditEvent::KillJob => "kill_job",
            AuditEvent::CancelJob => "cancel_job",
            AuditEvent::DeviceOffline => "device_offline",
            AuditEvent::DeviceBack => "device_back",
            AuditEvent::LeaseExpired => "lease_expired",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditRecord {
    ts: u64,
    event: String,
    user: String,
    uid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    device_uuids: Vec<String>,
    #[serde(default)]
    detail: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_jsonl_to_file() {
        let dir = std::env::temp_dir().join(format!("gpu-audit-test-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let log = AuditLog::new(Some(path.clone())).unwrap();
        log.record(AuditEvent::SubmitJob, "alice", 501, Some("j1"), &["gpu-0".into()], "sleep 10");
        log.record(AuditEvent::KillJob, "root", 0, Some("j1"), &[], "");
        drop(log);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"submit_job\""));
        assert!(lines[0].contains("\"user\":\"alice\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_audit_writes_nothing() {
        let log = AuditLog::new(None).unwrap();
        log.record(AuditEvent::JobQueued, "bob", 502, Some("j2"), &[], "");
        // 无文件可读,不 panic 即可
        assert!(log.file.lock().unwrap().is_none());
    }
}