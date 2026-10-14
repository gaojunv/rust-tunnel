//! Windows 图标嵌入：把 `icons/app.rc`（内含 `icon.ico`）编成资源并链进 exe。
//!
//! 只对 `*-pc-windows-gnu` 目标做事——本仓 GUI 的 Windows leg 只有这一个 target。
//! 其余目标（macOS / Linux，以及 MSVC）不需要资源文件，直接返回。

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/app.rc");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.ends_with("-pc-windows-gnu") {
        return;
    }

    if let Err(reason) = compile_resource_gnu(&target) {
        // 故意让构建失败，而不是降级成「没有图标的 exe」：走到这里说明在编 windows-gnu，
        // 而 mingw binutils 是链接的硬前提（windres 与链接器同一个包），它不在就是环境
        // 配错了。**静默降级正是这个 bug 藏了这么久的原因**——原先的实现在这里出错时
        // 只 eprintln 一句就继续，产出的 exe 少了图标却没人发现。
        eprintln!("windres (icon) 失败：{reason}");
        std::process::exit(1);
    }
}

/// 用 `windres` 把 `icons/app.rc` 编成 COFF 目标文件，经 `cargo:rustc-link-arg-bins`
/// 交给链接器。
///
/// **为什么不用 `embed-resource`**：它的后端在**编译期**由 build script 自身的
/// `#[cfg(target_env = ...)]` 选定，而 build script 是按 **host** 编译的。CI 在
/// `windows-latest`（host = `x86_64-pc-windows-msvc`）上交叉编 `x86_64-pc-windows-gnu`，
/// 于是它走 MSVC 分支、调 `rc.exe` 产出 MSVC 格式的 `app.lib`，mingw 的 `ld` 直接报
/// `file format not recognized`（GNU 分支本应产出 `libapp.a`）。TARGET 与 HOST 不一致时
/// 它的选择必然是错的，而本 leg 恰好永远不一致。
///
/// # Errors
/// `OUT_DIR` 读不到，或 `windres` 缺失、非零退出。
fn compile_resource_gnu(target: &str) -> Result<(), String> {
    // mingw-w64 的工具名带 target 前缀（`x86_64-w64-mingw32-`），binutils 装出来的就是
    // 这个名字。前缀由 TARGET 的 arch 推导，不写死，免得换个 arch 时静默找错工具。
    let arch = target.split('-').next().unwrap_or("x86_64");
    let windres = format!("{arch}-w64-mingw32-windres");

    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| format!("读不到 OUT_DIR：{e}"))?);
    let obj = out_dir.join("app-res.o");

    // 命令形状与 embed-resource 的 GNU 分支逐字一致——那是 host 为 gnu 时真正跑得通的
    // 那条路径，照抄它比自创参数安全。
    //   `--output-format=coff`：mingw 的 ld 只吃 COFF。
    //   `--include-dir icons`：`.rc` 里写的是裸文件名 `"icon.ico"`，显式给出它的目录，
    //   不依赖 windres 对输入文件所在目录的隐式搜索。
    check(
        Command::new(&windres)
            .args([
                "--input",
                "icons/app.rc",
                "--output-format=coff",
                "--include-dir",
                "icons",
            ])
            .arg("--output")
            .arg(&obj),
    )?;

    // 直接把 COFF 目标文件交给链接器（embed-resource 也这么做，只是给文件起了个 `.a` 的
    // 误导性名字）。单个 bin 不需要再打成静态库。
    println!("cargo:rustc-link-arg-bins={}", obj.display());
    Ok(())
}

/// 跑一个外部命令，非零退出即返回可读错误（附完整命令行，便于在 CI 日志里定位）。
fn check(cmd: &mut Command) -> Result<(), String> {
    let shown = format!("{cmd:?}");
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("`{shown}` 退出码 {status}")),
        Err(e) => Err(format!("无法执行 `{shown}`：{e}")),
    }
}
