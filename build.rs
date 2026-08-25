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
}
