//! 调度核心:优先级队列 + 队首阻塞放置 + 租约生命周期。
//!
//! 设计要点(与 DESIGN.md 对应):
//!
//! - **纯内存状态机**:时间由调用方注入([`Scheduler::tick`] 的 `now_ms`),
//!   保证可测试、可重放;内部操作返回 [`SchedulerEvent`] 事件流,由 daemon
//!   层负责落审计日志、持久化与通知(§7、§10)。
//! - **严格优先级 + 队首阻塞(head-of-line)**:只有队首作业尝试分配,放不下
//!   则整队等待——低优先级小作业不会插队抢占高优先级队首(§7.2)。
//! - **packing 放置**:按设备 UUID 稳定顺序取前 N 张空闲卡,碎片最小(§7.2)。
//! - **租约 TTL + 心跳续期**:过期未续期即回收(§6)。
//! - **快照/恢复**:[`SchedulerSnapshot`] 是全部内存态的可序列化形态,
//!   daemon 重启恢复用(M4,§10)。

use crate::error::SchedulerError;
use crate::model::{DeviceInfo, DeviceState, FailureReason, Job, JobStatus, Lease};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

/// 调度器配置
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)] // M5:旧版(M4 及更早)状态文件缺配额字段时以 Default(不限)补齐
pub struct SchedulerConfig {
    /// 排队超时秒数,超时自动取消(0 = 不限制)。默认 1h(ADR-2)
    pub queue_timeout_secs: u64,
    /// 租约 TTL 秒数,心跳续期刷新。默认 12h
    pub lease_ttl_secs: u64,
    /// M5 §7.3:每用户同时在飞(Allocated+Running)的最大卡数。0 = 不限。
    /// 超配额的新请求**入队等待**,且不阻塞其他用户(配额仅约束该用户自身)。
    pub max_gpus_per_user: u32,
    /// M5 §7.3:每用户非终态作业数上限。超限提交**直接拒绝**
    /// ([`SchedulerError::QuotaExceeded`])。0 = 不限。
    pub max_concurrent_jobs_per_user: u32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            queue_timeout_secs: 3600,
            lease_ttl_secs: 12 * 3600,
            max_gpus_per_user: 0,
            max_concurrent_jobs_per_user: 0,
        }
    }
}

/// 提交任务的请求
#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub owner: String,
    pub description: String,
    pub gpus_requested: u32,
    /// 优先级,越大越优先
    pub priority: u8,
    /// 覆盖默认排队超时
    pub queue_timeout_secs: Option<u64>,
}

/// 调度器对外事件:daemon 据此落审计日志 / 持久化 / 通知
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchedulerEvent {
    JobQueued {
        job_id: String,
    },
    JobAllocated {
        job_id: String,
        lease: Lease,
    },
    JobRunning {
        job_id: String,
    },
    JobFinished {
        job_id: String,
    },
    JobFailed {
        job_id: String,
        reason: FailureReason,
    },
    JobCancelled {
        job_id: String,
    },
    DeviceOffline {
        uuid: String,
    },
    DeviceBack {
        uuid: String,
    },
}

/// 队列项:BinaryHeap 最大堆,`greater = 更优先`
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueEntry {
    priority: u8,
    /// 提交序号:同优先级下先到先得
    seq: u64,
    job_id: String,
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap 是最大堆:"更大" = 更优先。
        // 高优先级更大;同优先级 seq 更小更大(严格 FIFO);再按 id 稳定。
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
            .then_with(|| self.job_id.cmp(&other.job_id))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 可持久化快照:调度器全部"记忆"的序列化形态(daemon 重启恢复用)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerSnapshot {
    pub config: SchedulerConfig,
    pub devices: BTreeMap<String, DeviceInfo>,
    pub jobs: HashMap<String, Job>,
    pub leases: HashMap<String, Lease>,
}

/// 调度核心。单写者:所有状态变更通过本类型的 `&mut` 方法进行。
/// (不实现 `Debug`:内部持有时钟闭包,无意义且不可派生)
pub struct Scheduler {
    config: SchedulerConfig,
    /// BTreeMap:稳定顺序,packing 放置确定性
    devices: BTreeMap<String, DeviceInfo>,
    jobs: HashMap<String, Job>,
    /// job_id -> lease(v1:一个作业一个租约)
    leases: HashMap<String, Lease>,
    queue: BinaryHeap<QueueEntry>,
    /// 时钟源注入(默认真实系统时钟;测试可覆盖)
    now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
    /// 提交序号计数器(严格 FIFO 的排序键)
    next_seq: u64,
}

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self::with_clock(config, system_now_ms)
    }

    /// 测试/重放用:注入时钟(可传闭包,便于确定性推进时间)
    pub fn with_clock(
        config: SchedulerConfig,
        now_ms: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            config,
            devices: BTreeMap::new(),
            jobs: HashMap::new(),
            leases: HashMap::new(),
            queue: BinaryHeap::new(),
            now_ms: Box::new(now_ms),
            next_seq: 1,
        }
    }

    // ---------- 设备管理 ----------

    /// 注册/刷新设备。新设备置 `Free`;已存在设备保留运行期状态,
    /// 仅更新静态信息(避免覆盖运行中的 Allocated/Offline 状态)。
    pub fn upsert_device(&mut self, info: DeviceInfo) {
        match self.devices.get_mut(&info.uuid) {
            Some(existing) => {
                existing.name = info.name;
                existing.pci_bus_id = info.pci_bus_id;
                existing.memory_total_mib = info.memory_total_mib;
            }
            None => {
                let mut info = info;
                info.state = DeviceState::Free;
                self.devices.insert(info.uuid.clone(), info);
            }
        }
    }

    pub fn device_state(&self, uuid: &str) -> Option<DeviceState> {
        self.devices.get(uuid).map(|d| d.state)
    }

    pub fn devices(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.devices.values()
    }

    pub fn free_count(&self) -> usize {
        self.devices
            .values()
            .filter(|d| d.state == DeviceState::Free)
            .count()
    }

    /// 设备故障下线:置 `Offline`,回收受影响的作业并释放其剩余设备
    pub fn device_offline(&mut self, uuid: &str) -> Result<Vec<SchedulerEvent>, SchedulerError> {
        let dev = self
            .devices
            .get_mut(uuid)
            .ok_or_else(|| SchedulerError::DeviceNotFound { uuid: uuid.into() })?;
        dev.state = DeviceState::Offline;

        let mut events = vec![SchedulerEvent::DeviceOffline { uuid: uuid.into() }];
        let affected: Vec<String> = self
            .leases
            .values()
            .filter(|l| l.device_uuids.iter().any(|u| u == uuid))
            .map(|l| l.job_id.clone())
            .collect();
        for job_id in affected {
            events.extend(self.release_job(&job_id, Some(FailureReason::DeviceOffline)));
        }
        Ok(events)
    }

    /// 设备故障恢复上线(Offline → Free)
    pub fn device_back(&mut self, uuid: &str) -> Result<Vec<SchedulerEvent>, SchedulerError> {
        let dev = self
            .devices
            .get_mut(uuid)
            .ok_or_else(|| SchedulerError::DeviceNotFound { uuid: uuid.into() })?;
        if dev.state != DeviceState::Offline {
            return Ok(vec![]);
        }
        dev.state = DeviceState::Free;
        Ok(vec![SchedulerEvent::DeviceBack { uuid: uuid.into() }])
    }

    // ---------- 作业生命周期 ----------

    /// 提交作业。资源充足立即分配,否则入队。返回 (job_id, 事件流)
    pub fn submit(
        &mut self,
        req: SubmitRequest,
    ) -> Result<(String, Vec<SchedulerEvent>), SchedulerError> {
        if req.gpus_requested == 0 {
            return Err(SchedulerError::InvalidRequest(
                "gpus_requested 必须 >= 1".into(),
            ));
        }
        if req.owner.trim().is_empty() {
            return Err(SchedulerError::InvalidRequest("owner 不能为空".into()));
        }

        // M5 §7.3:并发作业数配额(0 = 不限)——超限提交直接拒绝
        let max_concurrent = self.config.max_concurrent_jobs_per_user;
        if max_concurrent > 0 && self.user_active_jobs(&req.owner) >= max_concurrent as usize {
            return Err(SchedulerError::QuotaExceeded {
                owner: req.owner.clone(),
                detail: format!(
                    "同时并发作业数已达上限 {max_concurrent}(含排队与运行)"
                ),
            });
        }

        let now = (self.now_ms)();
        let job_id = uuid::Uuid::new_v4().to_string();
        let seq = self.next_seq;
        self.next_seq += 1;
        let job = Job {
            id: job_id.clone(),
            owner: req.owner,
            description: req.description,
            gpus_requested: req.gpus_requested,
            priority: req.priority,
            seq,
            status: JobStatus::Queued,
            failure: None,
            submitted_at_ms: now,
            queue_timeout_secs: req
                .queue_timeout_secs
                .unwrap_or(self.config.queue_timeout_secs),
        };

        // 先入队再尝试分配,保证"先到先得"语义可重放
        self.queue.push(QueueEntry {
            priority: job.priority,
            seq,
            job_id: job_id.clone(),
        });
        self.jobs.insert(job_id.clone(), job);

        let mut events = vec![SchedulerEvent::JobQueued {
            job_id: job_id.clone(),
        }];
        events.extend(self.try_allocate(now));
        Ok((job_id, events))
    }

    /// 取消排队中的作业(仅 `Queued`;运行中请用 [`Scheduler::kill`])
    pub fn cancel(&mut self, job_id: &str) -> Result<Vec<SchedulerEvent>, SchedulerError> {
        let job = self
            .jobs
            .get(job_id)
            .ok_or_else(|| SchedulerError::JobNotFound {
                job_id: job_id.into(),
            })?;
        if job.status != JobStatus::Queued {
            return Err(SchedulerError::InvalidJobState {
                job_id: job_id.into(),
                status: job.status,
            });
        }
        self.remove_from_queue(job_id);
        let job = self.jobs.get_mut(job_id).expect("存在性已校验");
        job.status = JobStatus::Failed;
        job.failure = Some(FailureReason::Cancelled);
        Ok(vec![SchedulerEvent::JobCancelled {
            job_id: job_id.into(),
        }])
    }

    /// Kill 运行中/已分配的作业:释放设备并置 `Failed(Killed)`。
    /// 授权(仅 root 与 job owner)由 daemon 层校验,此处只做状态机。
    pub fn kill(&mut self, job_id: &str) -> Result<Vec<SchedulerEvent>, SchedulerError> {
        let job = self
            .jobs
            .get(job_id)
            .ok_or_else(|| SchedulerError::JobNotFound {
                job_id: job_id.into(),
            })?;
        match job.status {
            JobStatus::Allocated | JobStatus::Running => {}
            other => {
                return Err(SchedulerError::InvalidJobState {
                    job_id: job_id.into(),
                    status: other,
                });
            }
        }
        Ok(self.release_job(job_id, Some(FailureReason::Killed)))
    }

    /// 作业结束:成功(`success = true` → `Succeeded`),或 daemon 观测到
    /// 进程非零退出/exec 失败(`success = false`,可携带原因)。
    pub fn finish(
        &mut self,
        job_id: &str,
        success: bool,
        reason: Option<FailureReason>,
    ) -> Result<Vec<SchedulerEvent>, SchedulerError> {
        let job = self
            .jobs
            .get(job_id)
            .ok_or_else(|| SchedulerError::JobNotFound {
                job_id: job_id.into(),
            })?;
        if !matches!(job.status, JobStatus::Allocated | JobStatus::Running) {
            return Err(SchedulerError::InvalidJobState {
                job_id: job_id.into(),
                status: job.status,
            });
        }
        let failure = if success {
            None
        } else {
            Some(reason.unwrap_or(FailureReason::Other("进程非零退出".into())))
        };
        Ok(self.release_job(job_id, failure))
    }

    /// 进程 exec 成功:Allocated → Running,并刷新租约心跳
    pub fn mark_running(&mut self, job_id: &str) -> Result<Vec<SchedulerEvent>, SchedulerError> {
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| SchedulerError::JobNotFound {
                job_id: job_id.into(),
            })?;
        if job.status != JobStatus::Allocated {
            return Err(SchedulerError::InvalidJobState {
                job_id: job_id.into(),
                status: job.status,
            });
        }
        job.status = JobStatus::Running;
        self.refresh_lease(job_id);
        Ok(vec![SchedulerEvent::JobRunning {
            job_id: job_id.into(),
        }])
    }

    /// 租约心跳续期(daemon 监控循环周期调用,对用户透明)
    pub fn heartbeat(&mut self, job_id: &str) -> Result<(), SchedulerError> {
        let job = self
            .jobs
            .get(job_id)
            .ok_or_else(|| SchedulerError::JobNotFound {
                job_id: job_id.into(),
            })?;
        if !matches!(job.status, JobStatus::Allocated | JobStatus::Running) {
            return Err(SchedulerError::InvalidJobState {
                job_id: job_id.into(),
                status: job.status,
            });
        }
        self.refresh_lease(job_id);
        Ok(())
    }

    /// 周期驱动:①排队超时取消 ②租约过期回收 ③尝试分配
    pub fn tick(&mut self, now_ms: u64) -> Vec<SchedulerEvent> {
        let mut events = Vec::new();

        // ① 排队超时
        let expired: Vec<String> = self
            .queue
            .iter()
            .filter(|e| {
                self.jobs
                    .get(&e.job_id)
                    .map(|j| {
                        j.queue_timeout_secs != 0
                            && j.submitted_at_ms
                                .saturating_add(j.queue_timeout_secs * 1000)
                                < now_ms
                    })
                    .unwrap_or(false)
            })
            .map(|e| e.job_id.clone())
            .collect();
        for job_id in expired {
            self.remove_from_queue(&job_id);
            if let Some(job) = self.jobs.get_mut(&job_id) {
                if job.status == JobStatus::Queued {
                    job.status = JobStatus::Failed;
                    job.failure = Some(FailureReason::QueueTimeout);
                    events.push(SchedulerEvent::JobFailed {
                        job_id,
                        reason: FailureReason::QueueTimeout,
                    });
                }
            }
        }

        // ② 租约过期
        let expired_leases: Vec<String> = self
            .leases
            .values()
            .filter(|l| l.expires_at_ms < now_ms)
            .map(|l| l.job_id.clone())
            .collect();
        for job_id in expired_leases {
            let running = self
                .jobs
                .get(&job_id)
                .map(|j| matches!(j.status, JobStatus::Allocated | JobStatus::Running))
                .unwrap_or(false);
            if running {
                events.extend(self.release_job(&job_id, Some(FailureReason::LeaseExpired)));
            }
        }

        // ③ 尝试分配
        events.extend(self.try_allocate(now_ms));
        events
    }

    // ---------- 查询 ----------

    pub fn job(&self, job_id: &str) -> Option<&Job> {
        self.jobs.get(job_id)
    }

    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    pub fn lease(&self, job_id: &str) -> Option<&Lease> {
        self.leases.get(job_id)
    }

    pub fn leases(&self) -> impl Iterator<Item = &Lease> {
        self.leases.values()
    }

    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    // ---------- 快照 / 恢复 ----------

    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            config: self.config,
            devices: self.devices.clone(),
            jobs: self.jobs.clone(),
            leases: self.leases.clone(),
        }
    }

    /// 从快照恢复:重建队列(按 `Queued` 作业的 seq 排序)并回到系统时钟
    pub fn from_snapshot(snap: SchedulerSnapshot) -> Self {
        let mut queue = BinaryHeap::new();
        let mut max_seq = 0u64;
        for job in snap.jobs.values() {
            max_seq = max_seq.max(job.seq);
            if job.status == JobStatus::Queued {
                queue.push(QueueEntry {
                    priority: job.priority,
                    seq: job.seq,
                    job_id: job.id.clone(),
                });
            }
        }
        Self {
            config: snap.config,
            devices: snap.devices,
            jobs: snap.jobs,
            leases: snap.leases,
            queue,
            now_ms: Box::new(system_now_ms),
            next_seq: max_seq + 1,
        }
    }

    // ---------- 内部实现 ----------

    /// 队首阻塞式分配:只尝试队首;放不下则停止(严格优先级,防插队)
    fn try_allocate(&mut self, now: u64) -> Vec<SchedulerEvent> {
        let mut events = Vec::new();
        // M5:因用户配额受限被跳过的作业,分配阶段结束后放回堆
        let mut deferred: Vec<QueueEntry> = Vec::new();
        loop {
            let Some(peeked) = self.queue.peek() else {
                break;
            };
            let Some(job) = self.jobs.get(&peeked.job_id) else {
                self.queue.pop();
                continue;
            };
            if job.status != JobStatus::Queued {
                // 防御:已取消/已终态的残留在队尾中,弹掉
                self.queue.pop();
                continue;
            }
            // M5 配额门禁:**先于**资源检查——配额是 per-user 硬门禁,
            // 不受全局资源余量影响(该用户本就不能再占)。
            // 配额受限 → 不分配,但**不阻塞其他用户**(配额只约束该用户自身,
            // 跳过继续看下一个;与低优先级插队无关)。
            let quota = self.config.max_gpus_per_user;
            if quota > 0 && self.user_inflight_gpus(&job.owner) + job.gpus_requested > quota {
                let skipped = self.queue.pop().expect("peek 已确认非空");
                deferred.push(skipped);
                continue;
            }
            if (self.free_count() as u32) < job.gpus_requested {
                // 队首都放不下,后面的不插队(M2 队首阻塞语义)
                break;
            }
            let entry = self.queue.pop().expect("peek 已确认非空");

            // packing:稳定顺序取前 N 张空闲卡
            let chosen: Vec<String> = self
                .devices
                .values()
                .filter(|d| d.state == DeviceState::Free)
                .take(job.gpus_requested as usize)
                .map(|d| d.uuid.clone())
                .collect();
            for u in &chosen {
                let dev = self.devices.get_mut(u).expect("设备存在");
                dev.state = DeviceState::Allocated;
            }

            let lease = Lease {
                id: uuid::Uuid::new_v4().to_string(),
                job_id: entry.job_id.clone(),
                device_uuids: chosen,
                issued_at_ms: now,
                expires_at_ms: now + self.config.lease_ttl_secs * 1000,
                heartbeat_at_ms: now,
            };
            self.leases.insert(entry.job_id.clone(), lease.clone());

            let job = self.jobs.get_mut(&entry.job_id).expect("作业存在");
            job.status = JobStatus::Allocated;

            events.push(SchedulerEvent::JobAllocated {
                job_id: entry.job_id,
                lease,
            });
        }
        for d in deferred {
            self.queue.push(d);
        }
        events
    }

    /// M5:用户当前在飞(Allocated+Running)占用的卡数,按租约设备数计
    fn user_inflight_gpus(&self, owner: &str) -> u32 {
        self.leases
            .values()
            .filter(|l| {
                self.jobs
                    .get(&l.job_id)
                    .map(|j| j.owner == owner)
                    .unwrap_or(false)
            })
            .map(|l| l.device_uuids.len() as u32)
            .sum()
    }

    /// M5:用户非终态作业数(提交并发配额检查用;含排队与运行)
    fn user_active_jobs(&self, owner: &str) -> usize {
        self.jobs
            .values()
            .filter(|j| j.owner == owner && !j.status.is_terminal())
            .count()
    }

    /// 释放作业占用的全部设备并置终态。`failure = None` → `Succeeded`,
    /// 否则 → `Failed(reason)`。
    fn release_job(&mut self, job_id: &str, failure: Option<FailureReason>) -> Vec<SchedulerEvent> {
        let mut events = Vec::new();

        if let Some(lease) = self.leases.remove(job_id) {
            for uuid in &lease.device_uuids {
                if let Some(dev) = self.devices.get_mut(uuid) {
                    // 设备处于 Offline(故障下线中)时不得被回拨为 Free
                    if dev.state != DeviceState::Offline {
                        dev.state = DeviceState::Free;
                    }
                }
            }
        }

        if let Some(job) = self.jobs.get_mut(job_id) {
            match failure {
                None => {
                    job.status = JobStatus::Succeeded;
                    events.push(SchedulerEvent::JobFinished {
                        job_id: job_id.into(),
                    });
                }
                Some(reason) => {
                    job.status = JobStatus::Failed;
                    job.failure = Some(reason.clone());
                    events.push(SchedulerEvent::JobFailed {
                        job_id: job_id.into(),
                        reason,
                    });
                }
            }
        }
        events
    }

    fn remove_from_queue(&mut self, job_id: &str) {
        self.queue = std::mem::take(&mut self.queue)
            .into_iter()
            .filter(|e| e.job_id != job_id)
            .collect();
    }

    fn refresh_lease(&mut self, job_id: &str) {
        let now = (self.now_ms)();
        if let Some(lease) = self.leases.get_mut(job_id) {
            lease.heartbeat_at_ms = now;
            lease.expires_at_ms = now + self.config.lease_ttl_secs * 1000;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    fn dev(uuid: &str) -> DeviceInfo {
        DeviceInfo {
            uuid: uuid.into(),
            name: "Test GPU".into(),
            pci_bus_id: "0000:00:00.0".into(),
            memory_total_mib: 16384,
            state: DeviceState::Free,
        }
    }

    fn empty_config() -> SchedulerConfig {
        SchedulerConfig::default()
    }

    /// 可控时钟:返回 (推进句柄, 时钟闭包),测试可确定性推进时间
    fn controlled_clock() -> (Arc<AtomicU64>, impl Fn() -> u64) {
        let now = Arc::new(AtomicU64::new(1_700_000_000_000));
        let handle = now.clone();
        (now, move || handle.load(Ordering::Relaxed))
    }

    fn submit(s: &mut Scheduler, owner: &str, gpus: u32, priority: u8) -> String {
        s.submit(SubmitRequest {
            owner: owner.into(),
            description: format!("{owner} job"),
            gpus_requested: gpus,
            priority,
            queue_timeout_secs: None,
        })
        .expect("提交成功")
        .0
    }

    #[test]
    fn allocates_immediately_when_free() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));
        s.upsert_device(dev("gpu-1"));

        let (id, events) = s
            .submit(SubmitRequest {
                owner: "alice".into(),
                description: "train".into(),
                gpus_requested: 2,
                priority: 5,
                queue_timeout_secs: None,
            })
            .unwrap();

        assert_eq!(s.job(&id).unwrap().status, JobStatus::Allocated);
        assert_eq!(s.free_count(), 0);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SchedulerEvent::JobAllocated { .. }))
        );
    }

    #[test]
    fn queues_when_busy_and_allocates_in_fifo_order() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));

        let blocker = submit(&mut s, "u1", 1, 5); // 立即分配
        let first = submit(&mut s, "u2", 1, 5); // 排队
        let second = submit(&mut s, "u3", 1, 5); // 排队
        assert_eq!(s.queued_count(), 2);
        assert_eq!(s.job(&first).unwrap().status, JobStatus::Queued);

        s.finish(&blocker, true, None).unwrap();
        let events = s.tick(system_now_ms());

        assert_eq!(s.job(&first).unwrap().status, JobStatus::Allocated);
        assert_eq!(s.job(&second).unwrap().status, JobStatus::Queued);
        assert!(
            events.iter().any(
                |e| matches!(e, SchedulerEvent::JobAllocated { job_id, .. } if job_id == &first)
            )
        );
    }

    #[test]
    fn priority_beats_fifo_order() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));

        let blocker = submit(&mut s, "u1", 1, 10);
        let low = submit(&mut s, "u2", 1, 1); // 先排队(低优先级)
        let high = submit(&mut s, "u3", 1, 10); // 后排队(高优先级)

        s.finish(&blocker, true, None).unwrap();
        s.tick(system_now_ms());

        assert_eq!(s.job(&high).unwrap().status, JobStatus::Allocated);
        assert_eq!(s.job(&low).unwrap().status, JobStatus::Queued);
    }

    #[test]
    fn head_of_line_blocks_lower_priority() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));

        let big = submit(&mut s, "u1", 2, 100); // 要 2 张,只有 1 张空闲 → 排队
        let small = submit(&mut s, "u2", 1, 1); // 放得下,但在队首之后 → 不得插队

        assert_eq!(s.job(&big).unwrap().status, JobStatus::Queued);
        assert_eq!(s.job(&small).unwrap().status, JobStatus::Queued);
        assert_eq!(s.free_count(), 1);

        // 补一张卡后:队首(big)先分配,small 仍等待
        s.upsert_device(dev("gpu-1"));
        s.tick(system_now_ms());
        assert_eq!(s.job(&big).unwrap().status, JobStatus::Allocated);
        assert_eq!(s.job(&small).unwrap().status, JobStatus::Queued);
        assert_eq!(s.free_count(), 0);
    }

    #[test]
    fn queue_timeout_expires_queued_job() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));

        let blocker = submit(&mut s, "u1", 1, 5);
        let id = s
            .submit(SubmitRequest {
                owner: "u2".into(),
                description: "timeout".into(),
                gpus_requested: 1,
                priority: 5,
                queue_timeout_secs: Some(1),
            })
            .unwrap()
            .0;
        assert_eq!(s.job(&id).unwrap().status, JobStatus::Queued);

        let events = s.tick(system_now_ms() + 2000);
        let job = s.job(&id).unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(job.failure, Some(FailureReason::QueueTimeout));
        assert!(events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::JobFailed { job_id, reason: FailureReason::QueueTimeout } if job_id == &id)));
        assert_eq!(s.queued_count(), 0);

        // 超时取消后立刻释放资源给后来的作业
        let later = submit(&mut s, "u3", 1, 5);
        assert_eq!(s.job(&later).unwrap().status, JobStatus::Queued);
        s.finish(&blocker, true, None).unwrap();
        s.tick(system_now_ms());
        assert_eq!(s.job(&later).unwrap().status, JobStatus::Allocated);
    }

    #[test]
    fn cancel_queued_job() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));
        let blocker = submit(&mut s, "u1", 1, 5);
        let queued = submit(&mut s, "u2", 1, 5);

        let events = s.cancel(&queued).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SchedulerEvent::JobCancelled { .. }))
        );
        let job = s.job(&queued).unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(job.failure, Some(FailureReason::Cancelled));
        assert_eq!(s.queued_count(), 0);

        // 后续 tick 不应复活已取消作业
        s.finish(&blocker, true, None).unwrap();
        s.tick(system_now_ms());
        assert_eq!(s.job(&queued).unwrap().status, JobStatus::Failed);
        assert_eq!(s.free_count(), 1);
    }

    #[test]
    fn cancel_allocated_job_is_rejected() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));
        let id = submit(&mut s, "u1", 1, 5);

        let err = s.cancel(&id).unwrap_err();
        assert!(matches!(err, SchedulerError::InvalidJobState { .. }));
    }

    #[test]
    fn kill_releases_devices_and_next_queued_gets_them() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));
        let a = submit(&mut s, "u1", 1, 5);
        let b = submit(&mut s, "u2", 1, 5);

        let events = s.kill(&a).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::JobFailed { job_id, reason: FailureReason::Killed } if job_id == &a)));
        assert_eq!(s.job(&a).unwrap().failure, Some(FailureReason::Killed));
        assert_eq!(s.free_count(), 1);

        // kill 本身不触发分配;下一次 tick 把资源给 b
        s.tick(system_now_ms());
        assert_eq!(s.job(&b).unwrap().status, JobStatus::Allocated);
    }

    #[test]
    fn finish_success_and_failure() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));
        let ok = submit(&mut s, "u1", 1, 5);
        s.mark_running(&ok).unwrap();

        let events = s.finish(&ok, true, None).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SchedulerEvent::JobFinished { .. }))
        );
        assert_eq!(s.job(&ok).unwrap().status, JobStatus::Succeeded);
        assert_eq!(s.lease(&ok), None);
        assert_eq!(s.free_count(), 1);

        let bad = submit(&mut s, "u2", 1, 5);
        s.finish(&bad, false, Some(FailureReason::Other("boom".into())))
            .unwrap();
        assert_eq!(s.job(&bad).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn device_offline_revokes_job_and_offline_never_allocates() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));
        let a = submit(&mut s, "u1", 1, 5);

        let events = s.device_offline("gpu-0").unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SchedulerEvent::DeviceOffline { .. }))
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::JobFailed { job_id, reason: FailureReason::DeviceOffline } if job_id == &a)));
        assert_eq!(s.device_state("gpu-0"), Some(DeviceState::Offline));
        assert_eq!(
            s.job(&a).unwrap().failure,
            Some(FailureReason::DeviceOffline)
        );
        assert_eq!(s.free_count(), 0);

        // 离线设备不参与分配
        let b = submit(&mut s, "u2", 1, 5);
        assert_eq!(s.job(&b).unwrap().status, JobStatus::Queued);

        // 恢复上线后可继续分配
        s.device_back("gpu-0").unwrap();
        s.tick(system_now_ms());
        assert_eq!(s.job(&b).unwrap().status, JobStatus::Allocated);
    }

    #[test]
    fn lease_expiry_fails_job_on_tick() {
        let config = SchedulerConfig {
            lease_ttl_secs: 1,
            ..Default::default()
        };
        let (clock_arc, clock) = controlled_clock();
        let mut s = Scheduler::with_clock(config, clock);
        s.upsert_device(dev("gpu-0"));
        let a = submit(&mut s, "u1", 1, 5);
        assert_eq!(s.job(&a).unwrap().status, JobStatus::Allocated);

        // 不续期,推进 2s(tick 时间显式注入,无需真实等待)
        clock_arc.store(clock_arc.load(Ordering::Relaxed) + 2000, Ordering::Relaxed);
        let events = s.tick(clock_arc.load(Ordering::Relaxed));
        assert!(events
            .iter()
            .any(|e| matches!(e, SchedulerEvent::JobFailed { job_id, reason: FailureReason::LeaseExpired } if job_id == &a)));
        assert_eq!(
            s.job(&a).unwrap().failure,
            Some(FailureReason::LeaseExpired)
        );
        assert_eq!(s.free_count(), 1);
    }

    #[test]
    fn heartbeat_renews_lease() {
        let config = SchedulerConfig {
            lease_ttl_secs: 1,
            ..Default::default()
        };
        let (clock_arc, clock) = controlled_clock();
        let mut s = Scheduler::with_clock(config, clock);
        s.upsert_device(dev("gpu-0"));
        let a = submit(&mut s, "u1", 1, 5);

        // 持续心跳,tick 推进远超过 TTL,租约永不失效
        let t0 = clock_arc.load(Ordering::Relaxed);
        for i in 1..=5u64 {
            clock_arc.store(t0 + i * 500, Ordering::Relaxed);
            s.heartbeat(&a).unwrap();
            s.tick(clock_arc.load(Ordering::Relaxed));
        }
        assert_eq!(s.job(&a).unwrap().status, JobStatus::Allocated);
        assert!(s.lease(&a).unwrap().expires_at_ms > clock_arc.load(Ordering::Relaxed));
    }

    #[test]
    fn rejects_invalid_requests() {
        let mut s = Scheduler::new(empty_config());
        let err = s
            .submit(SubmitRequest {
                owner: "u1".into(),
                description: "bad".into(),
                gpus_requested: 0,
                priority: 5,
                queue_timeout_secs: None,
            })
            .unwrap_err();
        assert!(matches!(err, SchedulerError::InvalidRequest(_)));
    }

    #[test]
    fn unknown_job_operations_error() {
        let mut s = Scheduler::new(empty_config());
        assert!(matches!(
            s.kill("nope"),
            Err(SchedulerError::JobNotFound { .. })
        ));
        assert!(matches!(
            s.cancel("nope"),
            Err(SchedulerError::JobNotFound { .. })
        ));
    }

    #[test]
    fn snapshot_roundtrip_recovers_state_and_queue() {
        let mut s = Scheduler::new(empty_config());
        s.upsert_device(dev("gpu-0"));
        s.upsert_device(dev("gpu-1"));
        s.upsert_device(dev("gpu-2"));
        let a = submit(&mut s, "u1", 1, 5); // 分配 gpu-0
        let b = submit(&mut s, "u2", 2, 5); // 分配 gpu-1, gpu-2
        let c = submit(&mut s, "u3", 1, 5); // 排队

        let snap = s.snapshot();

        // 模拟 daemon 重启:从快照恢复
        let mut restored = Scheduler::from_snapshot(snap);
        assert_eq!(restored.job(&a).unwrap().status, JobStatus::Allocated);
        assert_eq!(
            restored.lease(&a).unwrap().device_uuids,
            vec!["gpu-0".to_string()]
        );
        assert_eq!(restored.job(&b).unwrap().status, JobStatus::Allocated);
        assert_eq!(
            restored.lease(&b).unwrap().device_uuids,
            vec!["gpu-1".to_string(), "gpu-2".to_string()]
        );
        assert_eq!(restored.job(&c).unwrap().status, JobStatus::Queued);
        assert_eq!(restored.queued_count(), 1);
        assert_eq!(restored.free_count(), 0);

        // 恢复后继续推进:释放 a → 队首 c 拿到 gpu-0
        restored.finish(&a, true, None).unwrap();
        let t0 = Instant::now();
        loop {
            restored.tick(system_now_ms());
            if restored.job(&c).unwrap().status == JobStatus::Allocated {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(2), "分配卡死");
        }
        assert_eq!(
            restored.lease(&c).unwrap().device_uuids,
            vec!["gpu-0".to_string()]
        );
        assert_eq!(restored.free_count(), 0);
    }

    // ---------- M5 配额与公平(§7.3) ----------

    fn quota_config(max_gpus: u32, max_concurrent: u32) -> SchedulerConfig {
        SchedulerConfig {
            max_gpus_per_user: max_gpus,
            max_concurrent_jobs_per_user: max_concurrent,
            ..Default::default()
        }
    }

    #[test]
    fn m5_max_concurrent_jobs_rejects_oversubmission() {
        let mut s = Scheduler::new(quota_config(0, 1)); // 不限卡,但每用户最多 1 个作业
        s.upsert_device(dev("gpu-0"));

        let _a = submit(&mut s, "alice", 1, 5); // 占用配额
        match s.submit(SubmitRequest {
            owner: "alice".into(),
            description: "second".into(),
            gpus_requested: 1,
            priority: 5,
            queue_timeout_secs: None,
        }) {
            Err(SchedulerError::QuotaExceeded { owner, .. }) => assert_eq!(owner, "alice"),
            other => panic!("第 2 个并发作业应被拒绝,实际: {other:?}"),
        }
        // 其他用户不受影响
        let _b = submit(&mut s, "bob", 1, 5);
        assert!(
            s.queued_count() + s.leases.len() as usize >= 2,
            "bob 不受 alice 配额影响"
        );
    }

    #[test]
    fn m5_max_concurrent_zero_means_unlimited() {
        let mut s = Scheduler::new(quota_config(0, 0));
        s.upsert_device(dev("gpu-0"));
        let a = submit(&mut s, "alice", 1, 5);
        let b = submit(&mut s, "alice", 1, 5);
        assert!(a != b);
        assert_eq!(s.queued_count(), 1, "第 2 个作业排队而非拒绝(并发不限)");
    }

    /// 核心验收:alice 超"在飞卡数"配额时**不阻塞** bob 拿到卡;
    /// 而资源不足时仍保持队首阻塞(M2 语义)。
    #[test]
    fn m5_gpu_quota_skips_only_that_user_not_blocking_others() {
        let mut s = Scheduler::new(quota_config(1, 0)); // 每人最多同时 1 卡
        s.upsert_device(dev("gpu-0"));
        s.upsert_device(dev("gpu-1"));

        let alice_first = submit(&mut s, "alice", 1, 5); // 占 1 卡(已达配额)
        assert_eq!(s.job(&alice_first).unwrap().status, JobStatus::Allocated);
        assert_eq!(s.free_count(), 1);

        let alice_second = submit(&mut s, "alice", 1, 5); // 超配额 → 入队等待
        assert_eq!(s.job(&alice_second).unwrap().status, JobStatus::Queued);
        assert_eq!(s.queued_count(), 1);

        let bob = submit(&mut s, "bob", 1, 5); // bob 应跳过 alice 的配额作业拿到卡
        assert_eq!(
            s.job(&bob).unwrap().status,
            JobStatus::Allocated,
            "bob 不应被 alice 的配额等待阻塞"
        );
        assert_eq!(s.free_count(), 0, "剩余 1 卡应归 bob");
        // alice 的第二个作业仍排队
        assert_eq!(s.job(&alice_second).unwrap().status, JobStatus::Queued);
    }

    /// 配额释放后,等待中的作业自动分配(alice 一释放,b 立即拿到卡)
    #[test]
    fn m5_quota_waiter_allocated_after_user_frees_gpus() {
        let mut s = Scheduler::new(quota_config(1, 0));
        s.upsert_device(dev("gpu-0"));
        s.upsert_device(dev("gpu-1"));

        let alice_first = submit(&mut s, "alice", 1, 5);
        let alice_second = submit(&mut s, "alice", 1, 5); // 排队
        let _bob = submit(&mut s, "bob", 1, 5); // 拿第 2 卡
        assert_eq!(s.free_count(), 0);

        // alice 释放第一卡 → 配额空出一个 → alice_second 立即分配(先于后续作业)
        s.finish(&alice_first, true, None).unwrap();
        let events = s.tick(system_now_ms());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SchedulerEvent::JobAllocated { .. })),
            "释放后应触发分配"
        );
        assert_eq!(s.job(&alice_second).unwrap().status, JobStatus::Allocated);
        assert_eq!(s.free_count(), 0, "释放的卡应给 alice_second,保持满载");
    }

    /// 多请求叠加同样受配额约束(alice 2 卡请求在配额 1 下也等待)
    #[test]
    fn m5_quota_counts_requested_gpus() {
        let mut s = Scheduler::new(quota_config(1, 0));
        s.upsert_device(dev("gpu-0"));
        s.upsert_device(dev("gpu-1"));

        let _a = submit(&mut s, "alice", 1, 5);
        let b = submit(&mut s, "alice", 2, 5); // 2 卡 > 配额 1 → 即使有空卡也等待
        assert_eq!(s.job(&b).unwrap().status, JobStatus::Queued);
        assert_eq!(s.free_count(), 1, "配额 1 下 alice 不得再占卡");

        // 资源不足时其他用户仍可拿(配额只挡 alice)
        let c = submit(&mut s, "bob", 1, 5);
        assert_eq!(s.job(&c).unwrap().status, JobStatus::Allocated);
    }

    /// M2 回归:无配额或配额充足时,资源不足保持队首阻塞
    #[test]
    fn m5_head_of_line_still_blocks_on_resources() {
        let mut s = Scheduler::new(quota_config(0, 0));
        s.upsert_device(dev("gpu-0"));

        let _a = submit(&mut s, "u1", 1, 5); // 占唯一卡
        let big = submit(&mut s, "u2", 2, 5); // 队首:2 卡(放不下)
        let small = submit(&mut s, "u3", 1, 5); // 队尾:1 卡(能放下,但不插队)
        assert_eq!(s.job(&small).unwrap().status, JobStatus::Queued);

        s.finish(&_a, true, None).unwrap();
        s.tick(system_now_ms());
        // 队首 big 放不下(只剩 1 卡)→ 整队阻塞,small 不插队
        assert_eq!(s.job(&big).unwrap().status, JobStatus::Queued);
        assert_eq!(s.job(&small).unwrap().status, JobStatus::Queued);
        assert_eq!(s.free_count(), 1);
    }

    /// 快照序列化兼容:旧配置(无配额字段)读入后配额为 0(不限)
    #[test]
    fn m5_snapshot_missing_quota_fields_defaults_to_unlimited() {
        let old_json = r#"{
            "queue_timeout_secs": 3600,
            "lease_ttl_secs": 43200
        }"#;
        let cfg: SchedulerConfig = serde_json::from_str(old_json).unwrap();
        assert_eq!(cfg.max_gpus_per_user, 0, "旧快照应缺省为不限");
        assert_eq!(cfg.max_concurrent_jobs_per_user, 0);
    }
}
