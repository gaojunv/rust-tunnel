//! Windows 图标嵌入：把 `icons/app.rc`（内含 `icon.ico`）编成资源并链进 exe。
//!
//! 只对 `*-pc-windows-gnu` 目标做事——本仓 GUI 的 Windows leg 只有这一个 target。
//! 其余目标（macOS / Linux，以及 MSVC）不需要资源文件，直接返回。

use std::env;
use std::io::ErrorKind;
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
        // 而 mingw-w64 的 windres 是链接的硬前提。**静默降级正是这个 bug 藏了这么久的
        // 原因**——原先的实现在这里出错时只 eprintln 一句就继续，产出的 exe 少了图标
        // 却没人发现。
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
/// 两个候选名都跑不起来，或 `windres` 跑起来但非零退出，或读不到 `OUT_DIR`。
fn compile_resource_gnu(target: &str) -> Result<(), String> {
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| format!("读不到 OUT_DIR：{e}"))?);
    let obj = out_dir.join("app-res.o");

    let arch = target.split('-').next().unwrap_or("x86_64");
    // 资源编译器的名字**按环境而不同**，两个候选各对应一个真实环境：
    //   `x86_64-w64-mingw32-windres` —— Linux 发行版的 cross 工具链（`gcc-mingw-w64`）
    //   `windres`                    —— MSYS2 的 mingw64 环境（CI 的 windows leg 走这条）
    //
    // **别按「带前缀的名字一定存在」推**：MSYS2 的 gcc 包往 `/mingw64/bin` 里**两种名字
    // 都装**，binutils 包却**只装不带前缀的**（带前缀那份在 `/mingw64/x86_64-w64-mingw32/
    // bin/`，而那个目录不在 PATH 上）。于是 rustc 的 `x86_64-w64-mingw32-gcc` 找得到、
    // `x86_64-w64-mingw32-windres` 找不到——这个不对称来自两个包的打包方式不同，按它推
    // 会得出错误结论（第一版就是这么栽的，CI 报 `program not found`）。
    let candidates = [format!("{arch}-w64-mingw32-windres"), "windres".to_string()];

    let mut missing = Vec::new();
    for name in &candidates {
        let mut cmd = Command::new(name);
        //   `--output-format=coff`：mingw 的 ld 只吃 COFF。
        //   `--include-dir icons`：`.rc` 里写的是裸文件名 `"icon.ico"`，显式给出它的目录，
        //   不依赖 windres 对输入文件所在目录的隐式搜索。
        cmd.args([
            "--input",
            "icons/app.rc",
            "--output-format=coff",
            "--include-dir",
            "icons",
        ])
        .arg("--output")
        .arg(&obj);

        match cmd.status() {
            Ok(status) if status.success() => {
                // 直接把 COFF 目标文件交给链接器（embed-resource 也这么做，只是给文件起了个
                // `.a` 的误导性名字）。单个 bin 不需要再打成静态库。
                println!("cargo:rustc-link-arg-bins={}", obj.display());
                return Ok(());
            }
            // 跑起来了却失败：这是真错误（`.rc` 语法、找不到 icon.ico、格式不支持……），
            // 换个名字重试没有意义，直接把它报上去，别让第二个候选的错误盖住它。
            Ok(status) => return Err(format!("`{name}` 退出码 {status}；命令行：{cmd:?}")),
            Err(e) if e.kind() == ErrorKind::NotFound => missing.push(name.clone()),
            Err(e) => return Err(format!("无法执行 `{name}`：{e}")),
        }
    }

    Err(format!(
        "找不到资源编译器（试过 {missing:?}）。windows-gnu 目标需要 mingw-w64 的 windres，\
         它随 MSYS2 的 mingw-w64-x86_64-binutils 一起装（`pacman -S mingw-w64-x86_64-binutils`）；\
         请确认其 bin 目录在 PATH 上"
    ))
}
