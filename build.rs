fn main() {
    // Windows：把应用图标嵌入 exe 资源段。
    // 否则打包出的 exe 在资源管理器/任务栏显示通用空白图标。
    // 该文件内容见 assets/icons/app.rc。
    #[cfg(target_os = "windows")]
    {
        embed_resource::compile("assets/icons/app.rc", embed_resource::NONE)
            .manifest_required()
            .expect("embed-resource 编译应用图标失败");
    }

    // Linux：给可执行文件设置 RUNPATH，指向随包分发的 ONNX Runtime CUDA
    // provider 动态库（deb/appimage 布局：exe 在 <root>/usr/bin，资源在
    // <root>/usr/lib/screenshot-rs/）。ORT 在运行时用 dlopen 加载
    // libonnxruntime_providers_cuda.so，而 dlopen 会参与 DT_RUNPATH 搜索——
    // 不设则安装版找不到 provider 库，有 NVIDIA 显卡也无法用 GPU 推理。
    //
    // 同时生成 CUDA 运行库的 0 字节 stub 到 target/release/cuda-stubs/：
    // linuxdeploy 构建 AppImage 时会扫描 provider .so 的 NEEDED 依赖，构建机
    // 无 NVIDIA 导致 libcublas/libcudart 等缺失而中止（--exclude-library 对
    // 缺失库无效）；stub 让依赖在资源目录"找到"，AppImage 得以构建。运行时
    // 这些 stub 不会遮蔽客户机 NVIDIA 驱动提供的真库（AppRun 的
    // LD_LIBRARY_PATH 只含 usr/lib，不含 usr/lib/screenshot-rs 子目录）。
    // 0 字节非 ELF，linuxdeploy 部署时只复制不校验，无副作用。
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib/screenshot-rs");
        let stub_dir = std::path::Path::new("target/release/cuda-stubs");
        let _ = std::fs::create_dir_all(stub_dir);
        for name in [
            "libcublas.so.13",
            "libcublasLt.so.13",
            "libcurand.so.10",
            "libcudart.so.13",
            "libcuda.so.1",
        ] {
            let p = stub_dir.join(name);
            if !p.exists() {
                let _ = std::fs::write(&p, b"");
            }
        }
    }
}
