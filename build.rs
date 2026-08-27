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
    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib/screenshot-rs");
    }
}
