//! gpu-api —— V1 gRPC 接口:由 `proto/gpu.proto` 经 tonic-build 生成。
//!
//! 用法:`use gpu_api::grpc::{GpuManagerServer, GpuManagerClient, ...}`。

/// 生成的 protobuf 代码(含 tonic server/client trait,扁平结构)
pub mod pb {
    tonic::include_proto!("gpu.v1");
}

/// 便捷别名:`grpc::DeviceStatus`、`grpc::gpu_manager_server::GpuManagerServer` 等
pub use pb as grpc;
