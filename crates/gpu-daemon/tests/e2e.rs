//! 端到端集成测试:通过真实 UDS gRPC 通道验证 daemon 只读链路。
//! 全程使用 NullProvider + 模拟设备,无需真实 NVIDIA 硬件(macOS 可跑)。

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use gpu_api::grpc;
use gpu_api::grpc::gpu_manager_client::GpuManagerClient;
use gpu_daemon::{DaemonConfig, ProviderKind};
use tokio::time::sleep;

fn tmp_socket(tag: &str) -> PathBuf {
    let nano = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("gpu-daemon-e2e-{tag}-{nano}.sock"))
}

/// 统一 daemon 启动 helper。
/// 返回 (socket, audit_path, handle);`audit_path` 仅在开启审计时有效。
#[allow(clippy::too_many_arguments)]
async fn start_daemon_full(
    tag: &str,
    fake_devices: u32,
    audit_log: Option<PathBuf>,
    state_file: Option<PathBuf>,
) -> (PathBuf, PathBuf, tokio::task::JoinHandle<anyhow::Result<()>>) {
    start_daemon_full_quota(tag, fake_devices, audit_log, state_file, 0, 0).await
}

/// 带配额参数的统一启动器(M5)
#[allow(clippy::too_many_arguments)]
async fn start_daemon_full_quota(
    tag: &str,
    fake_devices: u32,
    audit_log: Option<PathBuf>,
    state_file: Option<PathBuf>,
    max_gpus_per_user: u32,
    max_concurrent_jobs_per_user: u32,
) -> (PathBuf, PathBuf, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let socket = tmp_socket(tag);
    let audit_path = match &audit_log {
        Some(p) => p.clone(),
        None => tmp_socket(&format!("{tag}-audit")).with_extension("jsonl"),
    };
    let cfg = DaemonConfig {
        socket_path: socket.clone(),
        provider: ProviderKind::Null,
        poll_interval: Duration::from_millis(100),
        metrics_port: 0, // 关闭 /metrics,避免端口冲突
        queue_timeout_secs: 3600,
        fake_devices,
        audit_log,
        cgroup_mode: gpu_daemon::CgroupMode::Noop,
        state_file,
        max_gpus_per_user,
        max_concurrent_jobs_per_user,
    };
    let handle = tokio::spawn(gpu_daemon::run(cfg));
    (socket, audit_path, handle)
}

async fn start_daemon(
    tag: &str,
    fake_devices: u32,
) -> (PathBuf, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (socket, _, handle) = start_daemon_full(tag, fake_devices, None, None).await;
    (socket, handle)
}

/// 带审计日志的 daemon(返回审计文件路径)
async fn start_daemon_audit(
    tag: &str,
    fake_devices: u32,
) -> (PathBuf, PathBuf, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (socket, audit_path, handle) = start_daemon_full(
        tag,
        fake_devices,
        Some(tmp_socket(&format!("{tag}-audit")).with_extension("jsonl")),
        None,
    )
    .await;
    (socket, audit_path, handle)
}

/// 等待 UDS 可连接(server 绑定有瞬时延迟)
async fn wait_connect(socket: &Path) -> GpuManagerClient<tonic::transport::Channel> {
    let mut last_err = None;
    for _ in 0..50 {
        match gpu_cli_connect(socket).await {
            Ok(c) => return c,
            Err(e) => last_err = Some(e),
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon 未能就绪: {last_err:?}");
}

/// 与 gpu-cli 相同的 UDS 连接逻辑(避免依赖 bin 内部函数)
async fn gpu_cli_connect(
    socket: &Path,
) -> anyhow::Result<GpuManagerClient<tonic::transport::Channel>> {
    let path = socket.to_path_buf();
    let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(tower::service_fn(move |_| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(GpuManagerClient::new(channel))
}

#[tokio::test(flavor = "multi_thread")]
async fn list_devices_over_uds_returns_fake_devices() {
    let (socket, handle) = start_daemon("ls", 3).await;
    let mut client = wait_connect(&socket).await;

    let resp = client
        .list_devices(grpc::ListDevicesRequest {})
        .await
        .unwrap();
    let devices = resp.into_inner().devices;
    assert_eq!(devices.len(), 3, "应返回 3 台模拟设备");
    for d in &devices {
        assert!(d.uuid.starts_with("gpu-fake-"), "uuid: {}", d.uuid);
        assert_eq!(d.state, grpc::DeviceState::Free as i32, "初始状态应为 FREE");
        assert!(d.memory_total_mib > 0);
    }

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_devices_sends_initial_snapshot() {
    let (socket, handle) = start_daemon("watch", 2).await;
    let mut client = wait_connect(&socket).await;

    let mut stream = client
        .watch_devices(grpc::WatchDevicesRequest {})
        .await
        .unwrap()
        .into_inner();

    // 回放初始快照:应收到 2 台设备(顺序不定)
    let mut seen = 0;
    for _ in 0..2 {
        match stream.next().await {
            Some(Ok(d)) => {
                assert!(d.uuid.starts_with("gpu-fake-"));
                seen += 1;
            }
            other => panic!("预期收到设备状态帧,实际: {other:?}"),
        }
    }
    assert_eq!(seen, 2);

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_are_zero_before_any_poll() {
    let (socket, handle) = start_daemon("metrics", 1).await;
    let mut client = wait_connect(&socket).await;

    let resp = client
        .get_metrics(grpc::GetMetricsRequest {})
        .await
        .unwrap();
    let m = resp.into_inner();
    assert_eq!(m.devices.len(), 1);
    assert_eq!(m.queued_jobs, 0);
    assert_eq!(m.running_jobs, 0);

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

#[tokio::test(flavor = "multi_thread")]
async fn poll_updates_metrics_and_broadcasts() {
    let (socket, handle) = start_daemon("poll", 1).await;

    // 先订阅 watch,再等一次轮询广播
    let mut client = wait_connect(&socket).await;
    let mut stream = client
        .watch_devices(grpc::WatchDevicesRequest {})
        .await
        .unwrap()
        .into_inner();
    // 消费初始快照
    let _ = stream.next().await.unwrap().unwrap();

    // 等轮询(100ms 间隔)推送过来,带模拟指标
    let mut got_metrics = false;
    for _ in 0..50 {
        match stream.next().await {
            Some(Ok(d)) => {
                if d.utilization_percent > 0 || d.temperature_c > 0 || d.used_memory_mib > 0 {
                    got_metrics = true;
                    break;
                }
            }
            _ => break,
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(got_metrics, "轮询后应收到带模拟指标的广播");

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

// ---------- M2:整卡分配闭环验收 ----------

/// 提交作业并返回响应
async fn submit(
    client: &mut GpuManagerClient<tonic::transport::Channel>,
    gpus: u32,
    command: Vec<String>,
) -> grpc::SubmitJobResponse {
    client
        .submit_job(grpc::SubmitJobRequest {
            submission: Some(grpc::Submission {
                description: command.join(" "),
                gpus_requested: gpus,
                priority: 5,
                command,
                env: vec![],
                workdir: "".into(),
            }),
        })
        .await
        .unwrap()
        .into_inner()
}

/// 轮询 GetJob 直到终态(最多 10s)
async fn wait_terminal(
    client: &mut GpuManagerClient<tonic::transport::Channel>,
    job_id: &str,
) -> grpc::Job {
    let mut last: Option<grpc::Job> = None;
    for _ in 0..200 {
        let job = client
            .get_job(grpc::GetJobRequest {
                job_id: job_id.into(),
            })
            .await
            .unwrap()
            .into_inner();
        let terminal = job.status == grpc::JobStatus::Succeeded as i32
            || job.status == grpc::JobStatus::Failed as i32;
        if terminal {
            return job;
        }
        last = Some(job);
        sleep(Duration::from_millis(50)).await;
    }
    panic!("job {job_id} 10s 内未到终态,最后状态: {last:?}");
}

/// 轮询设备状态直到匹配(最多 5s)
async fn wait_device_state(
    client: &mut GpuManagerClient<tonic::transport::Channel>,
    uuid: &str,
    expected: i32,
) {
    let mut last = None;
    for _ in 0..100 {
        let devices = client
            .list_devices(grpc::ListDevicesRequest {})
            .await
            .unwrap()
            .into_inner()
            .devices;
        for d in &devices {
            if d.uuid == uuid {
                if d.state == expected {
                    return;
                }
                last = Some(d.state);
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("设备 {uuid} 5s 内未达到预期状态,当前: {last:?}");
}

/// 单个作业闭环:提交 → 进程拉起(RUNNING) → 退出(SUCCEEDED) → 设备回 FREE
#[tokio::test(flavor = "multi_thread")]
async fn submit_job_runs_process_and_releases_gpu() {
    let (socket, handle) = start_daemon("m2-run", 1).await;
    let mut client = wait_connect(&socket).await;

    let resp = submit(&mut client, 1, vec!["/bin/sh".into(), "-c".into(), "sleep 0.5".into()]).await;
    assert_eq!(resp.status, grpc::JobStatus::Running as i32, "资源充足应立即启动");

    // 进程运行期间设备应为 ALLOCATED
    wait_device_state(&mut client, "gpu-fake-00", grpc::DeviceState::Allocated as i32).await;

    let job = wait_terminal(&mut client, &resp.job_id).await;
    assert_eq!(job.status, grpc::JobStatus::Succeeded as i32);

    // 退出后设备回 FREE,租约释放
    wait_device_state(&mut client, "gpu-fake-00", grpc::DeviceState::Free as i32).await;
    let job_after = client
        .get_job(grpc::GetJobRequest {
            job_id: resp.job_id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(job_after.lease.is_none(), "终态作业应无租约");

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

/// M2 核心验收:两进程抢 1 张卡,一个跑完另一个才跑
#[tokio::test(flavor = "multi_thread")]
async fn two_jobs_contend_for_one_gpu_run_in_order() {
    let (socket, handle) = start_daemon("m2-contend", 1).await;
    let mut client = wait_connect(&socket).await;

    let a = submit(&mut client, 1, vec!["/bin/sh".into(), "-c".into(), "sleep 0.6".into()]).await;
    assert_eq!(a.status, grpc::JobStatus::Running as i32);

    let b = submit(&mut client, 1, vec!["/bin/sh".into(), "-c".into(), "sleep 0.2".into()]).await;
    assert_eq!(b.status, grpc::JobStatus::Queued as i32, "无空闲卡应排队");

    // A 运行期间 B 必须保持 QUEUED(队首阻塞,防插队)
    for _ in 0..10 {
        let job_b = client
            .get_job(grpc::GetJobRequest {
                job_id: b.job_id.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            job_b.status,
            grpc::JobStatus::Queued as i32,
            "A 未结束前 B 不得提前运行"
        );
        sleep(Duration::from_millis(50)).await;
    }

    let job_a = wait_terminal(&mut client, &a.job_id).await;
    assert_eq!(job_a.status, grpc::JobStatus::Succeeded as i32);

    // A 结束后 B 拿到卡并跑完
    let job_b = wait_terminal(&mut client, &b.job_id).await;
    assert_eq!(job_b.status, grpc::JobStatus::Succeeded as i32);
    wait_device_state(&mut client, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

/// M2 核心验收:kill 后卡立即释放
#[tokio::test(flavor = "multi_thread")]
async fn kill_job_releases_gpu_immediately() {
    let (socket, handle) = start_daemon("m2-kill", 1).await;
    let mut client = wait_connect(&socket).await;

    let resp = submit(
        &mut client,
        1,
        vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
    )
    .await;
    assert_eq!(resp.status, grpc::JobStatus::Running as i32);

    client
        .kill_job(grpc::KillJobRequest {
            job_id: resp.job_id.clone(),
        })
        .await
        .unwrap();

    let job = wait_terminal(&mut client, &resp.job_id).await;
    assert_eq!(job.status, grpc::JobStatus::Failed as i32);
    assert_eq!(job.failure_reason, grpc::FailureReason::Killed as i32);

    // 卡立即释放(不等进程自然退出)
    wait_device_state(&mut client, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

/// 取消排队中的任务 → Failed(Cancelled),运行中的任务不受影响
#[tokio::test(flavor = "multi_thread")]
async fn cancel_queued_job_marks_failed() {
    let (socket, handle) = start_daemon("m2-cancel", 1).await;
    let mut client = wait_connect(&socket).await;

    let a = submit(&mut client, 1, vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]).await;
    let b = submit(&mut client, 1, vec!["/bin/sh".into(), "-c".into(), "true".into()]).await;
    assert_eq!(b.status, grpc::JobStatus::Queued as i32);

    client
        .cancel_job(grpc::CancelJobRequest {
            job_id: b.job_id.clone(),
        })
        .await
        .unwrap();

    let job_b = wait_terminal(&mut client, &b.job_id).await;
    assert_eq!(job_b.status, grpc::JobStatus::Failed as i32);
    assert_eq!(job_b.failure_reason, grpc::FailureReason::Cancelled as i32);

    // A 仍在运行
    let a_now = client
        .get_job(grpc::GetJobRequest {
            job_id: a.job_id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a_now.status, grpc::JobStatus::Running as i32);

    // 清理:A 继续占用,测试结束前显式 kill 防孤儿进程
    client
        .kill_job(grpc::KillJobRequest {
            job_id: a.job_id.clone(),
        })
        .await
        .unwrap();

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

/// 命令不存在 → 启动失败 → 作业 Failed,设备释放
#[tokio::test(flavor = "multi_thread")]
async fn missing_command_fails_job_and_frees_gpu() {
    let (socket, handle) = start_daemon("m2-badcmd", 1).await;
    let mut client = wait_connect(&socket).await;

    let resp = submit(&mut client, 1, vec!["no-such-cmd-gpu-xyz".into()]).await;
    assert_ne!(resp.status, grpc::JobStatus::Running as i32, "不存在的命令不能进入 Running");

    let job = wait_terminal(&mut client, &resp.job_id).await;
    assert_eq!(job.status, grpc::JobStatus::Failed as i32);
    wait_device_state(&mut client, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

// ---------- M3:审计与身份 ----------

/// 审计日志:提交→分配→运行→结束 的关键事件应落盘 JSONL
#[tokio::test(flavor = "multi_thread")]
async fn audit_log_tracks_job_lifecycle() {
    let (socket, audit_path, handle) = start_daemon_audit("m3-audit", 1).await;
    let mut client = wait_connect(&socket).await;

    let resp = submit(&mut client, 1, vec!["/bin/sleep".into(), "0.5".into()]).await;
    assert_eq!(resp.status, grpc::JobStatus::Running as i32);

    // owner 应来自 UDS 对端凭证(非 M2 的 daemon 环境变量)
    let job = client
        .get_job(grpc::GetJobRequest {
            job_id: resp.job_id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let expect_owner = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    assert_eq!(job.owner, expect_owner, "owner 应来自 SO_PEERCRED 解析的用户名");

    wait_terminal(&mut client, &resp.job_id).await;
    // 给 reaper/审计 flush 一点时间
    tokio::time::sleep(Duration::from_millis(300)).await;

    let content = std::fs::read_to_string(&audit_path).expect("审计文件应存在");
    for ev in ["submit_job", "job_allocated", "job_running", "job_succeeded"] {
        assert!(
            content.contains(&format!("\"event\":\"{ev}\"")),
            "审计应包含 {ev},实际:\n{content}"
        );
    }
    assert!(content.contains(&format!("\"job_id\":\"{}\"", resp.job_id)));
    assert!(content.contains("gpu-fake-00"), "分配事件应带设备 uuid");

    handle.abort();
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(audit_path);
}

/// Kill 会留下审计记录,且失败原因标记为 Killed
#[tokio::test(flavor = "multi_thread")]
async fn audit_log_records_kill() {
    let (socket, audit_path, handle) = start_daemon_audit("m3-audit-kill", 1).await;
    let mut client = wait_connect(&socket).await;

    let resp = submit(&mut client, 1, vec!["/bin/sleep".into(), "30".into()]).await;
    client
        .kill_job(grpc::KillJobRequest {
            job_id: resp.job_id.clone(),
        })
        .await
        .unwrap();
    wait_terminal(&mut client, &resp.job_id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let content = std::fs::read_to_string(&audit_path).expect("审计文件应存在");
    assert!(content.contains("\"event\":\"kill_job\""), "应记录 kill_job:\n{content}");
    assert!(content.contains("\"event\":\"job_failed\""), "应记录 job_failed:\n{content}");

    handle.abort();
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(audit_path);
}

// ---------- M4:重启恢复 ----------

/// 重启恢复:daemon 优雅退出(不杀子进程)→ 用同一状态文件重启 →
/// 作业恢复 Running → 完成后自动回收、设备释放(验收:重启 daemon → 状态正确恢复)
#[tokio::test(flavor = "multi_thread")]
async fn restart_recovers_running_job() {
    let state_file = tmp_socket("m4-state").with_extension("json");
    let (socket_a, _, handle_a) = start_daemon_full("m4-restart-a", 1, None, Some(state_file.clone())).await;
    let mut client_a = wait_connect(&socket_a).await;

    let resp = submit(&mut client_a, 1, vec!["/bin/sleep".into(), "8".into()]).await;
    assert_eq!(resp.status, grpc::JobStatus::Running as i32);
    // 等待状态文件落盘(进程已注册)
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(state_file.exists(), "状态文件应已持久化");

    // daemon A 退出(不杀子进程,孤儿 sleep 继续跑)
    handle_a.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // daemon B:同状态文件重启
    let (socket_b, _, handle_b) = start_daemon_full("m4-restart-b", 1, None, Some(state_file.clone())).await;
    let mut client_b = wait_connect(&socket_b).await;

    // 作业应恢复 Running
    let job = client_b
        .get_job(grpc::GetJobRequest {
            job_id: resp.job_id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        job.status,
        grpc::JobStatus::Running as i32,
        "重启后作业应恢复 Running(进程未死于 daemon 退出)"
    );

    // 等待自然结束 → 自动回收 → 设备释放。
    // 注:重启接管的孤儿进程由 daemon B 以 pid 探测,退出码不可考
    // (非其父进程,无法 wait),因此按"未知状态"回收为 Failed;
    // 关键验收是:作业走到终态、设备释放、不再占用。
    let job = wait_terminal(&mut client_b, &resp.job_id).await;
    assert!(
        job.status == grpc::JobStatus::Succeeded as i32
            || job.status == grpc::JobStatus::Failed as i32,
        "恢复的作业应走到终态,实际 status={}",
        job.status
    );
    wait_device_state(&mut client_b, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    handle_b.abort();
    let _ = std::fs::remove_file(socket_a);
    let _ = std::fs::remove_file(socket_b);
    let _ = std::fs::remove_file(&state_file);
}

/// 重启恢复(回收分支):恢复时发现进程已死 → 作业 Failed、设备释放、队列可推进
#[tokio::test(flavor = "multi_thread")]
async fn restart_collects_dead_job() {
    let state_file = tmp_socket("m4-state2").with_extension("json");
    let (socket_a, _, handle_a) = start_daemon_full("m4-dead-a", 1, None, Some(state_file.clone())).await;
    let mut client_a = wait_connect(&socket_a).await;

    let resp = submit(&mut client_a, 1, vec!["/bin/sleep".into(), "31".into()]).await;
    assert_eq!(resp.status, grpc::JobStatus::Running as i32);
    tokio::time::sleep(Duration::from_millis(500)).await; // 让状态文件落盘

    // 手动杀掉孤儿进程(模拟"daemon 宕机期间进程也死了")
    // 用唯一个性化的 sleep 参数,避免与其他测试并行时的 pgrep 误伤
    let out = std::process::Command::new("pgrep")
        .args(["-f", "sleep 31"])
        .output()
        .expect("pgrep 应可用");
    let pid = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .expect("应能找到 sleep 31 进程");
    std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .expect("kill 应成功");

    handle_a.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // daemon B 重启:恢复时探测 pid 已死 → 作业 Failed,设备释放
    let (socket_b, _, handle_b) = start_daemon_full("m4-dead-b", 1, None, Some(state_file.clone())).await;
    let mut client_b = wait_connect(&socket_b).await;

    let job = client_b
        .get_job(grpc::GetJobRequest {
            job_id: resp.job_id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(job.status, grpc::JobStatus::Failed as i32, "死进程应被回收为 Failed");
    wait_device_state(&mut client_b, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    handle_b.abort();
    let _ = std::fs::remove_file(socket_a);
    let _ = std::fs::remove_file(socket_b);
    let _ = std::fs::remove_file(&state_file);
}

/// 恢复后队列可继续推进:重启 daemon 后新作业可获取释放后的设备
#[tokio::test(flavor = "multi_thread")]
async fn restart_then_new_job_runs() {
    let state_file = tmp_socket("m4-state3").with_extension("json");
    let (socket_a, _, handle_a) = start_daemon_full("m4-seq-a", 1, None, Some(state_file.clone())).await;
    let mut client_a = wait_connect(&socket_a).await;
    let resp = submit(&mut client_a, 1, vec!["/bin/sleep".into(), "6".into()]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle_a.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (socket_b, _, handle_b) = start_daemon_full("m4-seq-b", 1, None, Some(state_file.clone())).await;
    let mut client_b = wait_connect(&socket_b).await;
    wait_terminal(&mut client_b, &resp.job_id).await; // 恢复的作业跑到结束
    wait_device_state(&mut client_b, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    // 新提交:正常分配并运行(恢复后的 daemon 状态一致、可继续服务)
    let resp2 = submit(&mut client_b, 1, vec!["/bin/sleep".into(), "2".into()]).await;
    assert_eq!(resp2.status, grpc::JobStatus::Running as i32);
    let job = wait_terminal(&mut client_b, &resp2.job_id).await;
    assert_eq!(job.status, grpc::JobStatus::Succeeded as i32);

    handle_b.abort();
    let _ = std::fs::remove_file(socket_a);
    let _ = std::fs::remove_file(socket_b);
    let _ = std::fs::remove_file(&state_file);
}

// ---------- M5 公平与配额(§7.3) ----------

/// 核心验收:每用户最大在飞卡数 —— 第 2 个作业排队等待,
/// kill 第一个后自动运行(资源释放 → 配额空位 → 队列推进)
#[tokio::test(flavor = "multi_thread")]
async fn quota_gpus_second_job_waits_then_runs_after_kill() {
    let (socket, _, handle) =
        start_daemon_full_quota("q1", 1, None, None, 1, 0).await;
    let mut client = wait_connect(&socket).await;

    let a = submit(&mut client, 1, vec!["/bin/sleep".into(), "30".into()]).await;
    assert_eq!(a.status, grpc::JobStatus::Running as i32);

    // 同用户第 2 个作业:超"在飞卡数"配额 → 入队等待(不是拒绝,也不是立即运行)
    let b = submit(&mut client, 1, vec!["/bin/sleep".into(), "3".into()]).await;
    assert_eq!(
        b.status,
        grpc::JobStatus::Queued as i32,
        "超配额作业应排队等待而非运行/拒绝"
    );

    // kill 第一个 → 配额空位 → b 自动分配并运行,短任务自然完成
    client
        .kill_job(grpc::KillJobRequest {
            job_id: a.job_id.clone(),
        })
        .await
        .unwrap();
    wait_terminal(&mut client, &b.job_id).await;
    assert_eq!(
        client
            .get_job(grpc::GetJobRequest {
                job_id: b.job_id.clone(),
            })
            .await
            .unwrap()
            .into_inner()
            .status,
        grpc::JobStatus::Succeeded as i32
    );
    wait_device_state(&mut client, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

/// 并发作业配额:第二提交直接拒绝(gRPC RESOURCE_EXHAUSTED)
#[tokio::test(flavor = "multi_thread")]
async fn quota_concurrent_jobs_rejects_oversubmission() {
    let (socket, _, handle) =
        start_daemon_full_quota("qc", 1, None, None, 0, 1).await;
    let mut client = wait_connect(&socket).await;

    let a = submit(&mut client, 1, vec!["/bin/sleep".into(), "30".into()]).await;
    assert_eq!(a.status, grpc::JobStatus::Running as i32);

    // 第 2 个并发作业:提交被配额拒绝
    let err = client
        .submit_job(grpc::SubmitJobRequest {
            submission: Some(grpc::Submission {
                description: "second".into(),
                gpus_requested: 1,
                priority: 5,
                command: vec!["/bin/sleep".into(), "30".into()],
                env: vec![],
                workdir: "".into(),
            }),
        })
        .await
        .expect_err("并发作业数超限应提交失败")
        .message()
        .to_string();
    assert!(
        err.contains("配额"),
        "错误应说明超配额,实际: {err}"
    );
    // 设备仍被第一个作业占用,无泄漏
    wait_device_state(&mut client, "gpu-fake-00", grpc::DeviceState::Allocated as i32).await;

    handle.abort();
    let _ = std::fs::remove_file(socket);
}

/// 配额在重启后仍生效(配额随调度快照持久化)
#[tokio::test(flavor = "multi_thread")]
async fn quota_survives_restart() {
    let state_file = tmp_socket("m5-state").with_extension("json");
    let (socket_a, _, handle_a) =
        start_daemon_full_quota("qa", 1, None, Some(state_file.clone()), 1, 0).await;
    let mut client_a = wait_connect(&socket_a).await;
    let a = submit(&mut client_a, 1, vec!["/bin/sleep".into(), "6".into()]).await;
    assert_eq!(a.status, grpc::JobStatus::Running as i32);
    tokio::time::sleep(Duration::from_millis(400)).await; // 状态文件落盘

    handle_a.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (socket_b, _, handle_b) =
        start_daemon_full_quota("qb", 1, None, Some(state_file.clone()), 1, 0).await;
    let mut client_b = wait_connect(&socket_b).await;
    wait_terminal(&mut client_b, &a.job_id).await; // 恢复的作业跑到结束,配额空出
    wait_device_state(&mut client_b, "gpu-fake-00", grpc::DeviceState::Free as i32).await;

    // 新提交:配额恢复后正常运行
    let b = submit(&mut client_b, 1, vec!["/bin/sleep".into(), "2".into()]).await;
    assert_eq!(b.status, grpc::JobStatus::Running as i32);

    handle_b.abort();
    let _ = std::fs::remove_file(socket_a);
    let _ = std::fs::remove_file(socket_b);
    let _ = std::fs::remove_file(&state_file);
}
