// 仅 Windows 下把鼠标图标编进 exe 资源（资源 id = 1）。
// 非 Windows 平台（如本机做交叉检查）不执行，避免拉取无关依赖。
#[cfg(windows)]
fn main() {
    if let Err(e) = winresource::WindowsResource::new()
        .set_icon("mouse.ico")
        .compile()
    {
        eprintln!("嵌入图标失败: {}", e);
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {}
