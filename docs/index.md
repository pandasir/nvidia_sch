---
layout: home

hero:
  name: GPU Manager
  text: 把 GPU 分给该跑的任务，跑完后自动收回来
  tagline: 单机整卡 GPU 调度与隔离服务。支持优先级排队、cgroup v2 隔离、审计、配额与重启恢复；没有 NVIDIA GPU 也能用 Null Provider 跑通完整流程。
  actions:
    - theme: brand
      text: 下载 v0.1.0
      link: /download/
    - theme: alt
      text: 3 分钟体验
      link: /guide/quick-start/

features:
  - title: 整卡独占
    details: 一张 GPU 同一时刻只属于一个任务，避免多人抢卡和显存互相干扰。
  - title: 强制隔离
    details: CUDA_VISIBLE_DEVICES 注入软约束，cgroup v2 设备过滤提供硬隔离。
  - title: 可恢复、可审计
    details: 作业状态持久化、进程回收、JSONL 审计日志与 Prometheus 指标。
---

## 3 分钟跑通一次调度

不需要 NVIDIA 驱动。使用 Null Provider 模拟三张卡，就能验证排队、分配、运行和回收的完整闭环。

```bash
mkdir -p /tmp/gpu-manager

# 终端 A：启动 daemon
gpu-daemon \
  --socket-path /tmp/gpu-manager/gpu.sock \
  --provider null \
  --fake-devices 3 \
  --cgroup-mode noop

# 终端 B：查看设备、提交任务、查看作业
SOCK=/tmp/gpu-manager/gpu.sock

gpu --socket-path "$SOCK" ls
gpu --socket-path "$SOCK" run --gpus=1 -- sleep 10
gpu --socket-path "$SOCK" jobs
```

完整步骤和真机部署方式见 [快速开始](/guide/quick-start/)。

## 工作原理

```text
+----------------+
|    gpu CLI     |
+-------+--------+
        | gRPC over Unix Socket
        v
+----------------+
| API / Scheduler|
+-------+--------+
        | 分配决策
        v
+----------------+     +----------------------+
|    Enforcer    | --> | CUDA_VISIBLE_DEVICES |
|   cgroup v2    |     |  GPU device nodes    |
+-------+--------+     +----------------------+
        | NVML / Null Provider
        v
+----------------+
|  NVIDIA GPU(s) |
+----------------+
```

调度核心只做决策；进程启动、设备隔离和状态回收由外层编排完成。Null Provider 演示时用模拟设备替换最底层。

## v1 边界

GPU Manager v1 是**单机、整卡独占**的资源管理服务。

- 支持：NVML 设备发现、整卡分配、优先级队列、配额、审计、指标与状态恢复
- 不支持：MIG 切分、CUDA MPS 分片、多机集群调度与运行任务抢占
- 尚未落地：调度策略 `spread`、公平份额与 aging、pty 交互直通
- 真机 cgroup v2 硬隔离和 Xid 事件链路仍建议在目标环境复核

这些边界不是“即将可用”的同义词；它们当前不属于 v1 的已实现能力。
