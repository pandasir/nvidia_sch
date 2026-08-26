//! 编译 proto/gpu.proto 生成 tonic/prost 代码。
//!
//! protoc 解析顺序:环境变量 `PROTOC` → vendored 预编译 protoc(跨平台、免安装)
//! → 系统 PATH 中的 `protoc`。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../../proto/gpu.proto");

    if std::env::var("PROTOC").is_err() {
        if let Ok(p) = protoc_bin_vendored::protoc_bin_path() {
            // edition 2024:set_var 为 unsafe(build script 单线程环境,安全)
            unsafe { std::env::set_var("PROTOC", p) };
        }
        // 若 vendored 不可用(下载失败),回退到系统 PATH 中的 protoc
    }

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let proto_dir = manifest_dir.join("../../proto");
    let proto_file = proto_dir.join("gpu.proto");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto_file], &[proto_dir])
        .expect("gpu.proto 编译失败");
}
