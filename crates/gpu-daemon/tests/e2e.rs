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

async fn start_daemon(
    tag: &str,
    fake_devices: u32,
) -> (PathBuf, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let socket = tmp_socket(tag);
    let cfg = DaemonConfig {
        socket_path: socket.clone(),
        provider: ProviderKind::Null,
        poll_interval: Duration::from_millis(100),
        metrics_port: 0, // 关闭 /metrics,避免端口冲突
        queue_timeout_secs: 3600,
        fake_devices,
    };
    let handle = tokio::spawn(gpu_daemon::run(cfg));
    (socket, handle)
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
async fn job_rpcs_are_not_yet_implemented() {
    let (socket, handle) = start_daemon("job", 1).await;
    let mut client = wait_connect(&socket).await;

    let err = client
        .submit_job(grpc::SubmitJobRequest {
            submission: Some(grpc::Submission {
                description: "x".into(),
                gpus_requested: 1,
                priority: 5,
                command: vec!["true".into()],
                env: vec![],
                workdir: "".into(),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unimplemented);

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
