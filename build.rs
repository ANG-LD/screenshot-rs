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
    // 同时生成 CUDA 运行库的**最小 ELF stub** 到 target/release/cuda-stubs/：
    // linuxdeploy 构建 AppImage 时扫描 provider .so 的 NEEDED 依赖，构建机无
    // NVIDIA 导致 libcublas/libcudart 等缺失而中止（--exclude-library 对缺失
    // 库无效）。CI 把 stub 注入宿主机 /usr/lib/x86_64-linux-gnu/（见
    // packaging.yml）让依赖解析通过，配合 excluded-libraries 不部署进
    // AppImage；运行时 CUDA 库由客户机 NVIDIA 驱动提供。stub 必须是合法 ELF
    // 共享库（0 字节文件 ldd 报 "file too short"），内容为空壳即可。
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib/screenshot-rs");
        let stub_dir = std::path::Path::new("target/release/cuda-stubs");
        let _ = std::fs::create_dir_all(stub_dir);
        let src_file = stub_dir.join("_stub.c");
        let _ = std::fs::write(&src_file, "int ort_cuda_stub_placeholder;\n");
        for name in [
            "libcublas.so.13",
            "libcublasLt.so.13",
            "libcurand.so.10",
            "libcudart.so.13",
            "libcuda.so.1",
        ] {
            let out = stub_dir.join(name);
            let _ = std::process::Command::new("cc")
                .args(["-shared", "-fPIC", "-x", "c"])
                .arg(&src_file)
                .arg("-o")
                .arg(&out)
                .arg(format!("-Wl,-soname,{name}"))
                .status();
        }
        let _ = std::fs::remove_file(&src_file);
    }
}
