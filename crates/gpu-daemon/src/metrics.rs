//! Prometheus /metrics 文本端点(纯 std 实现,无第三方 HTTP 依赖)。
//!
//! 仅监听 127.0.0.1,格式为 Prometheus exposition text 0.0.4。
//! 后续如需更丰富的指标(TLS/压缩等)再替换为 axum/hyper。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use crate::state::AppState;

/// 启动微 HTTP 服务,提供 GET /metrics。
pub fn spawn_metrics_server(
    port: u16,
    state: Arc<AppState>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    Ok(std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = state.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // 读取并丢弃请求头(M1 不做路由解析)
                let body = render_metrics(&state);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    }))
}

fn render_metrics(state: &AppState) -> String {
    let mut out = String::new();
    let statuses = state.status_list();
    let (queued, running) = state.scheduler_counts();

    out.push_str("# HELP gpu_info 设备静态信息与调度状态\n# TYPE gpu_info gauge\n");
    for d in &statuses {
        let st = match d.state {
            x if x == gpu_api::grpc::DeviceState::Free as i32 => "free",
            x if x == gpu_api::grpc::DeviceState::Allocated as i32 => "allocated",
            _ => "offline",
        };
        out.push_str(&format!(
            "gpu_info{{uuid=\"{}\",name=\"{}\",pci_bus_id=\"{}\",state=\"{}\"}} 1\n",
            escape(&d.uuid),
            escape(&d.name),
            escape(&d.pci_bus_id),
            st
        ));
    }

    out.push_str("# HELP gpu_memory_total_mib 显存总量\n# TYPE gpu_memory_total_mib gauge\n");
    for d in &statuses {
        out.push_str(&format!(
            "gpu_memory_total_mib{{uuid=\"{}\"}} {}\n",
            escape(&d.uuid),
            d.memory_total_mib
        ));
    }

    out.push_str("# HELP gpu_memory_used_mib 已用显存(MiB)\n# TYPE gpu_memory_used_mib gauge\n");
    for d in &statuses {
        out.push_str(&format!(
            "gpu_memory_used_mib{{uuid=\"{}\"}} {}\n",
            escape(&d.uuid),
            d.used_memory_mib
        ));
    }

    out.push_str(
        "# HELP gpu_utilization_percent GPU 利用率(0-100)\n# TYPE gpu_utilization_percent gauge\n",
    );
    for d in &statuses {
        out.push_str(&format!(
            "gpu_utilization_percent{{uuid=\"{}\"}} {}\n",
            escape(&d.uuid),
            d.utilization_percent
        ));
    }

    out.push_str("# HELP gpu_temperature_c 温度(摄氏度)\n# TYPE gpu_temperature_c gauge\n");
    for d in &statuses {
        out.push_str(&format!(
            "gpu_temperature_c{{uuid=\"{}\"}} {}\n",
            escape(&d.uuid),
            d.temperature_c
        ));
    }

    out.push_str("# HELP gpu_queued_jobs 排队任务数\n# TYPE gpu_queued_jobs gauge\n");
    out.push_str(&format!("gpu_queued_jobs {queued}\n"));
    out.push_str("# HELP gpu_running_jobs 运行中任务数\n# TYPE gpu_running_jobs gauge\n");
    out.push_str(&format!("gpu_running_jobs {running}\n"));

    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
