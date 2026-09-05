//! Windows 图标嵌入：仅在 Windows 目标上编译 `icons/app.rc`（内含 `icon.ico`）。

fn main() {
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/app.rc");

    #[cfg(windows)]
    {
        if let Err(e) = embed_resource::compile("icons/app.rc", embed_resource::NONE)
            .manifest_optional()
        {
            eprintln!("embed-resource (icon) failed (ignored): {e}");
        }
    }
}
