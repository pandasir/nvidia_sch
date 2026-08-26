# GPU Manager (Rust)

单机 NVIDIA GPU 调度与分配服务:调度/分配/回收本机显卡资源,供本机用户直接运行进程,以**整卡独占**粒度起步。

> 完整设计文档见 [`DESIGN.md`](DESIGN.md):架构分层、租约模型、强制层(device cgroup)、监控故障、里程碑与 ADR 均已定稿。

## 当前进度:M1 设备发现与监控 ✅

- **gpu-core**(纯状态机,零 I/O):优先级队列、队首阻塞放置、租约 TTL + 心跳、排队超时、设备下线/恢复、Kill/取消、快照恢复 — 15 个单测
- **gpu-api**:`proto/gpu.proto` → tonic/prost 代码生成(vendored protoc,免安装)
- **gpu-daemon**:监控轮询(NVML 同步 FFI 隔离在 spawn_blocking)+ gRPC over UDS(List/Watch/GetMetrics)+ Prometheus `/metrics`(纯 std 实现,无 HTTP 依赖)
- **nvmlx**:`DeviceProvider` 抽象;`NullProvider` 模拟指标(无卡可跑);`NvmlProvider` 真机实现(运行时加载驱动库,无驱动报 Init 错误)
- **gpu-cli**:`gpu ls` / `gpu watch` 可用;`run`/`jobs`/`queue`/`kill` 在 M2 接入
- 5 个端到端集成测试:真实 UDS gRPC 通道 + NullProvider 模拟设备,覆盖 ls/watch/轮询广播/未实现 RPC

## 构建与测试(无需 GPU)

```bash
cargo build --workspace
cargo test  --workspace
```

## 快速上手(无 GPU 机器演示)

```bash
# 终端 1:启动 daemon(3 台模拟设备,2s 一轮指标刷新)
cargo run -p gpu-daemon -- --provider null --fake-devices 3 --poll-interval-secs 2

# 终端 2:查看设备实时状态
cargo run -p gpu-cli -- ls

# 实时刷新(监控广播)
cargo run -p gpu-cli -- watch

# Prometheus 指标
curl -s http://127.0.0.1:9900/metrics
```

## 目录

```
crates/gpu-core     调度核心:领域模型 + 纯状态机(零 I/O,全量单测)
crates/nvmlx        NVML 访问抽象:DeviceProvider trait + NullProvider(mock)
crates/gpu-daemon   守护进程:入口/编排(API 服务 M2 接入)
crates/gpu-cli      CLI 客户端骨架(子命令面已定)
proto/              V1 gRPC 接口契约(M2 接入代码生成)
```

## 里程碑

| 里程碑 | 状态 |
|---|---|
| M0 骨架(workspace + proto + crate 划分) | ✅ |
| M1 设备发现与监控(NVML 枚举、轮询、/metrics) | ✅ |
| M2 整卡分配闭环(SubmitJob → 启动 → 退出回收) | ⏳ |
| M3 强制与安全(device cgroup、setuid、pty、审计) | ⏳ |
| M4 故障与恢复(Xid 订阅、设备下线、重启恢复) | ⏳ |
| M5 公平与配额 | ⏳ |