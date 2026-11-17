# 下载

<ReleaseDownload />

## 平台兼容

| 平台 | 推荐方式 | 要求 |
|---|---|---|
| Linux x86-64 | 使用上方预编译归档 | glibc >= 2.34；动态链接 NVIDIA 驱动提供的 `libnvidia-ml.so`（真机模式） |
| macOS / 其他平台 | 从源码构建 | Rust 1.85+；`protoc` 由构建脚本 vendored 提供 |

预编译二进制只面向 Linux amd64。其他平台请走源码构建，不要把 Linux 归档当作该平台的推荐安装方式。

## 从源码构建

```bash
git clone https://github.com/pandasir/nvidia_sch.git
cd nvidia_sch

cargo build --release --workspace --bins
cargo test --workspace
```

构建产物位于 `target/release/gpu-daemon` 与 `target/release/gpu`。

更多安装说明见 [README](https://github.com/pandasir/nvidia_sch#readme)。
