use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

// 依存ライブラリの名前
const LIB_NAME: &str = "libvpx";
const LINK_NAME: &str = "vpx";

// シンボル書き換え用のプレフィックス
//
// prebuilt で配布する際、他のライブラリが同じ libvpx シンボル (vpx_codec_encode 等) を
// 使っていると衝突する。この定数のプレフィックスを付けることで回避する。
//
// 変換例:
//   vpx_codec_encode → shiguredo_vpx_codec_encode (vpx_ を shiguredo_vpx_ に置換)
//   vp8_denoiser_free → shiguredo_vpx_vp8_denoiser_free (内部シンボルは単純にプレフィックス付与)
const SYMBOL_PREFIX: &str = "shiguredo_vpx";

// ── Android 交叉编译 ────────────────────────────────────────────────
//
// Android **没有预编译产物**（shiguredo 的 release 只覆盖 linux/macos/windows 六种组合），
// 所以 `should_use_prebuilt()` 对 `target_os = "android"` 恒返回 false，直接走源码构建。
// 依赖方（`crates/remote`）**不需要**显式打开 `source-build` feature，桌面依赖图因此
// 逐字节不变（仍然只有默认 feature 的 prebuilt 路径）。
//
// 工具链来源按优先级：
//   1. cargo-ndk 注入的 `CC_<target-triple>` / `CXX_<target-triple>` /
//      `AR_<target-triple>` / `CFLAGS_<target-triple>`（命名与 `cc` crate 一致：
//      连字符原样与下划线两种写法都认）；
//   2. 只设置了 `ANDROID_NDK_HOME` 时，从 `<ndk>/toolchains/llvm/prebuilt/<host>/bin`
//      取 `clang` / `clang++` / `llvm-ar`，并自行补 `--target=<clang-triple><api>`。
//
// 为什么不是 `CC_<libvpx-target>`：libvpx 1.16 的 configure（`build/make/configure.sh`
// 里的 `setup_gnu_toolchain`）只读普通的 `CC`/`CXX`/`AR`/`AS`/`LD`/`STRIP`。而
// `arm64-android-gcc` 这种名字带连字符，POSIX shell 根本无法用 `$VAR` 展开，所以必须
// 由这里做一层映射。
//
// 构建前置（缺一不可）：
//   - git（构建期 clone libvpx v1.16.0 源码）
//   - make
//   - NDK 的 clang / clang++ / llvm-ar
//   - ARM 目标还需要 perl（`build/make/ads2gas.pl` 把 ARM 汇编转成 GAS 语法）
//   - x86_64 / x86 模拟器目标还需要 host 上的 nasm 或 yasm（libvpx 的 x86 汇编是 NASM
//     语法，NDK 只带 clang/llvm，不含汇编器）
//   - rustup 的 llvm-tools 组件（构建后要用 llvm-nm / llvm-objcopy 改写符号）
//
// 完整命令行见仓库根目录 `scripts/build-android-libvpx.sh`；细节见同目录 README 的
// 「Android」一节。

fn main() {
    // Cargo.toml か build.rs が更新されたら、依存ライブラリを再ビルドする
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_SOURCE_BUILD");
    println!("cargo::rerun-if-env-changed=LIBVPX_TARGET");
    // Android 交叉编译：工具链既可能来自 cargo-ndk 的按 triple 变量，也可能只来自
    // ANDROID_NDK_HOME。任何一个变化都必须让 build.rs 重跑。
    println!("cargo::rerun-if-env-changed=ANDROID_NDK_HOME");
    println!("cargo::rerun-if-env-changed=ANDROID_NDK_ROOT");
    println!("cargo::rerun-if-env-changed=ANDROID_NDK");
    println!("cargo::rerun-if-env-changed=NDK_HOME");
    println!("cargo::rerun-if-env-changed=ANDROID_PLATFORM");
    println!("cargo::rerun-if-env-changed=CARGO_NDK_PLATFORM");
    if let Ok(target) = env::var("TARGET") {
        for base in [
            "CC", "CXX", "AR", "AS", "LD", "STRIP", "CFLAGS", "CXXFLAGS", "RANLIB",
        ] {
            println!("cargo::rerun-if-env-changed={base}_{target}");
            println!(
                "cargo::rerun-if-env-changed={base}_{}",
                target.replace('-', "_")
            );
        }
    }

    // 各種変数やビルドディレクトリのセットアップ
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("infallible"));
    let output_metadata_path = out_dir.join("metadata.rs");
    let output_bindings_path = out_dir.join("bindings.rs");

    // 各種メタデータを書き込む
    let (url, version) = get_url_and_version();
    fs::write(
        output_metadata_path,
        format!(
            concat!(
                "pub const BUILD_METADATA_REPOSITORY: &str={:?};\n",
                "pub const BUILD_METADATA_VERSION: &str={:?};\n",
            ),
            url, version
        ),
    )
    .expect("failed to write metadata file");

    if env::var("DOCS_RS").is_ok() {
        // Docs.rs 向けのビルドでは git clone ができないので build.rs の処理はスキップして、
        // 代わりに、ドキュメント生成時に最低限必要な定義だけをダミーで出力している。
        //
        // See also: https://docs.rs/about/builds
        fs::write(
            output_bindings_path,
            concat!(
                "pub struct vpx_codec_iface;",
                "pub struct vpx_codec_cx_pkt__bindgen_ty_1__bindgen_ty_1;",
                "pub struct vpx_codec_enc_cfg;",
                "pub struct vpx_image;",
                "pub struct vpx_codec_iter_t;",
                "pub struct vpx_codec_ctx;",
                "pub struct vpx_codec_err_t;",
            ),
        )
        .expect("write file error");
        return;
    }

    let output_lib_dir = if should_use_prebuilt() {
        download_prebuilt(&out_dir)
    } else {
        build_from_source(&out_dir, &output_bindings_path)
    };

    println!("cargo::rustc-link-search={}", output_lib_dir.display());
    println!("cargo::rustc-link-lib=static={LINK_NAME}");
}

// source-build feature が有効でなければ prebuilt を使う。
// ただし Android には prebuilt が存在しないため、常にソースビルドへ回す
// （依赖方无需打开 source-build，桌面 feature 集合因此不变）。
fn should_use_prebuilt() -> bool {
    if env::var("CARGO_FEATURE_SOURCE_BUILD").is_ok() {
        return false;
    }
    !is_android()
}

/// ターゲット OS が Android かどうか
fn is_android() -> bool {
    env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "android")
}

// prebuilt バイナリをダウンロードして展開する
fn download_prebuilt(out_dir: &Path) -> PathBuf {
    let target = get_target_platform();
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is not set");
    let base_url = format!(
        "https://github.com/shiguredo/libvpx-rs/releases/download/{}",
        version
    );
    let archive_name = format!("libvpx-{}.tar.gz", target);
    let archive_url = format!("{}/{}", base_url, archive_name);
    let sha256_url = format!("{}/{}.sha256", base_url, archive_name);

    let archive_path = out_dir.join("prebuilt.tar.gz");
    let sha256_path = out_dir.join("prebuilt.sha256");
    let prebuilt_dir = out_dir.join("prebuilt");
    fs::create_dir_all(&prebuilt_dir).expect("failed to create prebuilt directory");

    // curl でアーカイブをダウンロード
    eprintln!("prebuilt ライブラリをダウンロード中: {}", archive_url);
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&archive_path)
        .arg(&archive_url)
        .status()
        .expect("failed to execute curl. Ensure curl is installed");
    if !status.success() {
        panic!("failed to download prebuilt library: {}", archive_url);
    }

    // curl で SHA256 チェックサムをダウンロード
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&sha256_path)
        .arg(&sha256_url)
        .status()
        .expect("failed to execute curl");
    if !status.success() {
        panic!("failed to download SHA256 checksum: {}", sha256_url);
    }

    // SHA256 を検証
    verify_sha256(&archive_path, &sha256_path);

    // tar で展開
    let status = Command::new("tar")
        .args(["xzf"])
        .arg(&archive_path)
        .arg("-C")
        .arg(&prebuilt_dir)
        .status()
        .expect("failed to execute tar. Ensure tar is installed");
    if !status.success() {
        panic!("failed to extract prebuilt archive");
    }

    // ライブラリファイルを OUT_DIR/lib/ にコピー
    //
    // prebuilt アーカイブにはリリースビルド時にシンボル書き換え済みの libvpx.a と
    // #[link_name] 属性付きの bindings.rs が含まれているため、そのままコピーするだけでよい。
    let lib_dir = out_dir.join("lib");
    fs::create_dir_all(&lib_dir).expect("failed to create lib directory");
    fs::copy(
        prebuilt_dir.join("lib").join("libvpx.a"),
        lib_dir.join("libvpx.a"),
    )
    .expect("failed to copy libvpx.a");

    // bindings.rs を OUT_DIR/ にコピー
    fs::copy(
        prebuilt_dir.join("bindings.rs"),
        out_dir.join("bindings.rs"),
    )
    .expect("failed to copy bindings.rs");

    lib_dir
}

// SHA256 チェックサムを検証する
fn verify_sha256(file_path: &Path, sha256_path: &Path) {
    let expected = fs::read_to_string(sha256_path)
        .expect("failed to read SHA256 checksum file")
        .split_whitespace()
        .next()
        .expect("SHA256 checksum file is empty")
        .to_lowercase();

    let actual = compute_sha256(file_path);
    if actual != expected {
        panic!(
            "SHA256 checksum mismatch:\n  expected: {}\n  actual:   {}",
            expected, actual
        );
    }
    eprintln!("SHA256 checksum verified: {}", actual);
}

// ファイルの SHA256 ハッシュを計算する
fn compute_sha256(path: &Path) -> String {
    let output = if cfg!(target_os = "macos") {
        // macOS: shasum を使用
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
            .expect("failed to execute shasum. Ensure shasum is installed")
    } else if cfg!(target_os = "windows") {
        // Windows: certutil を使用
        Command::new("certutil")
            .args(["-hashfile"])
            .arg(path)
            .arg("SHA256")
            .output()
            .expect("failed to execute certutil")
    } else {
        // Linux: sha256sum を使用
        Command::new("sha256sum")
            .arg(path)
            .output()
            .expect("failed to execute sha256sum. Ensure coreutils is installed")
    };

    if !output.status.success() {
        panic!("failed to compute SHA256 checksum");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if cfg!(target_os = "windows") {
        // certutil 出力形式:
        // SHA256 hash of <file>:
        // <hash>
        // CertUtil: -hashfile command completed successfully.
        stdout
            .lines()
            .nth(1)
            .expect("unexpected certutil output format")
            .trim()
            .to_lowercase()
    } else {
        // shasum / sha256sum 出力形式: <hash>  <filename>
        stdout
            .split_whitespace()
            .next()
            .expect("unexpected shasum/sha256sum output format")
            .to_lowercase()
    }
}

// ソースからビルドする
fn build_from_source(out_dir: &Path, output_bindings_path: &Path) -> PathBuf {
    // Android は git clone の前にツールチェーンを検証する（数百 MB の clone を
    // 待たせてから「NDK が無い」と落ちるのは最悪の体験なので）。
    let android_toolchain = is_android().then(detect_android_toolchain);

    let out_build_dir = out_dir.join("build/");
    let src_dir = out_build_dir.join(LIB_NAME);
    let input_header_dir = src_dir.join("include/vpx/");
    let output_lib_dir = src_dir.join("lib/");
    let _ = fs::remove_dir_all(&out_build_dir);
    // Windows 上 `git clone` 出来的 `.git/objects` 是只读的，`remove_dir_all` 会静默
    // 失败（随后 `create_dir` 报 183 AlreadyExists）。这里做两层兜底：
    //   1. 半成品（没有 configure）整体改名挪开，而不是硬删；
    //   2. 已存在的完整源码树直接复用（configure/make 会重跑，省一次几百 MB 的 clone）。
    if out_build_dir.exists() && !src_dir.join("configure").is_file() {
        let stale = out_dir.join(format!("build-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&stale);
        let _ = fs::rename(&out_build_dir, &stale);
        let _ = fs::remove_dir_all(&stale);
    }
    fs::create_dir_all(&out_build_dir).expect("failed to create build directory");

    // 依存ライブラリのリポジトリを取得する（既に取得済みなら再利用する）
    if src_dir.join("configure").is_file() {
        eprintln!(
            "libvpx の既存ソースツリーを再利用します: {}",
            src_dir.display()
        );
    } else {
        git_clone_external_lib(&out_build_dir);
    }

    // ソースからビルドする
    build_from_source_platform(&src_dir, android_toolchain.as_ref());

    // 静的ライブラリのシンボルを書き換える
    let callbacks = rewrite_symbols(&output_lib_dir, out_dir);

    // バインディングを生成する
    //
    // parse_callbacks にシンボル書き換え用の ParseCallbacks を渡すことで、
    // 生成されるバインディングに #[link_name = "書き換え後のシンボル名"] が自動付与される。
    // Android：自动指向 NDK 自带的 libclang（未安装独立 LLVM 时 bindgen 找不到）。
    if let Some(toolchain) = android_toolchain.as_ref() {
        if env::var_os("LIBCLANG_PATH").is_none() {
            if let Some(dir) = toolchain.libclang_dir.as_deref() {
                // SAFETY: build script 是单线程进程，bindgen 在同一进程内读取该变量，
                // 不存在并发访问环境的风险。
                unsafe { env::set_var("LIBCLANG_PATH", dir) };
            }
        }
    }

    let mut builder = bindgen::Builder::default()
        .header(input_header_dir.join("vp8cx.h").display().to_string())
        .header(input_header_dir.join("vp8dx.h").display().to_string())
        .header(input_header_dir.join("vpx_codec.h").display().to_string())
        .header(input_header_dir.join("vpx_decoder.h").display().to_string())
        .header(input_header_dir.join("vpx_encoder.h").display().to_string())
        .parse_callbacks(Box::new(callbacks));
    if let Some(toolchain) = android_toolchain.as_ref() {
        for arg in &toolchain.bindgen_args {
            builder = builder.clang_arg(arg.clone());
        }
    }
    builder
        .generate()
        .expect("failed to generate bindings")
        .write_to_file(output_bindings_path)
        .expect("failed to write bindings");

    output_lib_dir
}

// プラットフォームに応じてソースビルドの処理を分岐する
fn build_from_source_platform(src_dir: &Path, android_toolchain: Option<&AndroidToolchain>) {
    if let Some(toolchain) = android_toolchain {
        build_from_source_android(src_dir, toolchain);
        return;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        build_from_source_windows(src_dir);
    } else {
        build_from_source_unix(src_dir);
    }
}

// ── Android 源码构建 ────────────────────────────────────────────────

/// Android 工具链与 libvpx 目标名
struct AndroidToolchain {
    /// libvpx configure 的 `--target=` 值（`arm64-android-gcc` 等）
    libvpx_target: &'static str,
    /// 需要注入给 configure / make 的环境变量（CC/CXX/AR/AS/CFLAGS...）
    envs: Vec<(String, String)>,
    /// bindgen 的 clang 参数（`--target` / `--sysroot` / `--resource-dir`）。
    /// NDK 自带的是通用 libclang，不知道目标三元组，也不自动找 sysroot 里的
    /// `<stdint.h>`，不显式传就会在解析 `vpx_integer.h` 时报 `'stddef.h' file not found`。
    bindgen_args: Vec<String>,
    /// NDK 的 libclang 所在目录（`.../prebuilt/<host>/bin`），用于自动设置
    /// `LIBCLANG_PATH`。
    libclang_dir: Option<PathBuf>,
}

/// (libvpx target 名, NDK clang 的 `--target=` 三元组前缀)
fn android_libvpx_target(arch: &str) -> Option<(&'static str, &'static str)> {
    match arch {
        "aarch64" => Some(("arm64-android-gcc", "aarch64-linux-android")),
        // NDK 的 32 位 ARM 三元组是 `armv7a-linux-androideabi`（Rust 的是
        // `armv7-linux-androideabi`），clang 的 `--target=` 必须用前者。
        "arm" => Some(("armv7-android-gcc", "armv7a-linux-androideabi")),
        "x86_64" => Some(("x86_64-android-gcc", "x86_64-linux-android")),
        "x86" => Some(("x86-android-gcc", "i686-linux-android")),
        _ => None,
    }
}

/// Android 源码ビルド
fn build_from_source_android(src_dir: &Path, toolchain: &AndroidToolchain) {
    eprintln!(
        "Android 目标 {}：从源码构建 libvpx（--target={}）",
        env::var("TARGET").unwrap_or_default(),
        toolchain.libvpx_target
    );

    // 4:4:4 profile 1 依赖 `--enable-vp9-highbitdepth`（两个既有参数都不能删）；
    // Android 只需要静态库，因此关掉示例/工具/单元测试，避免交叉链接（也避开
    // NDK 不带 x86 汇编器时的额外麻烦）。
    let configure_args = [
        format!("--target={}", toolchain.libvpx_target),
        "--disable-shared".to_string(),
        "--enable-vp9-highbitdepth".to_string(),
        "--disable-examples".to_string(),
        "--disable-tools".to_string(),
        "--disable-unit-tests".to_string(),
        "--disable-docs".to_string(),
    ];

    run_android_configure(src_dir, &configure_args, &toolchain.envs);
    run_android_make(src_dir, &toolchain.envs, &[]);
    run_android_make(src_dir, &toolchain.envs, &["install"]);
}

/// `configure` を実行する
///
/// Windows ホストでは configure は POSIX shell スクリプトなので sh 経由で起動し、
/// prefix は `$(pwd)` で展開する（`F:\...` のバックスラッシュが sh に食われるのを避ける）。
fn run_android_configure(src_dir: &Path, args: &[String], envs: &[(String, String)]) {
    let mut command = if cfg!(windows) {
        // 必须用**绝对 Windows 路径**调用 configure：它用 `${0%/*}` 推导
        // `source_path`，再写进 `libs-<target>.mk` 的 `SRC_PATH_BARE`。传 `./configure`
        // 会让它回退到 MSYS 的 `/f/...`，而原生 mingw32-make 不认这种路径，
        // 报 "No rule to make target '/f/.../libs.mk'"。
        let configure = src_dir.to_string_lossy().replace('\\', "/");
        let mut cmdline = format!("sh \"{configure}/configure\"");
        for arg in args {
            // 引数はすべて固定フラグ（空白を含まない）なので、そのまま連結してよい
            cmdline.push(' ');
            cmdline.push_str(arg);
        }
        // 原生 make（mingw32-make）不认 `/f/...` 这种 MSYS 路径：configure 会把
        // prefix 写进 config.mk 的 `SRC_PATH_BARE`，随后 makefile 用绝对路径
        // include `libs.mk`，POSIX 路径会让 make 报 "No such file or directory"。
        // `pwd -W` 输出 `F:/...` 形式的 Windows 路径（MSYS/Cygwin 都支持）。
        cmdline.push_str(" --prefix=\"$(pwd -W)\"");
        let mut command = Command::new("sh");
        command.arg("-c").arg(cmdline);
        command
    } else {
        let mut command = Command::new("./configure");
        command
            .args(args)
            .arg(format!("--prefix={}", src_dir.display()));
        command
    };
    command.current_dir(src_dir);
    for (key, value) in envs {
        command.env(key, value);
    }

    let success = command.status().is_ok_and(|status| status.success());
    if !success {
        panic!(
            "[configure] Android 源码构建 libvpx 失败（target={}）。\
             请确认：git / make 可用、NDK 工具链路径正确（路径中不要有空格）、\
             ARM 目标装有 perl（ads2gas.pl 依赖）、x86_64 目标装有 nasm 或 yasm。",
            env::var("TARGET").unwrap_or_default()
        );
    }
}

/// `make` / `make install` を実行する
fn run_android_make(src_dir: &Path, envs: &[(String, String)], args: &[&str]) {
    let jobs = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2)
        .clamp(1, 8);
    let mut command = Command::new("make");
    command
        .arg(format!("-j{jobs}"))
        .args(args)
        .current_dir(src_dir);
    for (key, value) in envs {
        command.env(key, value);
    }

    let step = if args.is_empty() {
        "make"
    } else {
        "make install"
    };
    let success = command.status().is_ok_and(|status| status.success());
    if !success {
        panic!(
            "[{step}] Android 源码构建 libvpx 失败（target={}）。\
             请确认 git / make 可用且 NDK 工具链完整。",
            env::var("TARGET").unwrap_or_default()
        );
    }
}

/// Android ツールチェーンを検出する
///
/// 見つからない場合は「何を設定すればよいか」を含む中国語メッセージで panic する。
fn detect_android_toolchain() -> AndroidToolchain {
    let rust_target = env::var("TARGET").expect("TARGET is not set");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let Some((libvpx_target, clang_triple)) = android_libvpx_target(&arch) else {
        panic!(
            "不支持的 Android 架构：arch={arch}（target={rust_target}）。\
             目前支持 aarch64（arm64-v8a）、arm（armeabi-v7a）、x86_64、x86。"
        );
    };
    let api = android_api_level();

    // 1) cargo-ndk 注入的按 target triple 命名的变量（与 cc crate 同一套查找顺序）
    let mut cc = lookup_target_env("CC", &rust_target);
    let mut cxx = lookup_target_env("CXX", &rust_target);
    let mut ar = lookup_target_env("AR", &rust_target);
    let mut cflags = lookup_target_env("CFLAGS", &rust_target);
    let mut cxxflags = lookup_target_env("CXXFLAGS", &rust_target);

    // 2) 退回到 ANDROID_NDK_HOME 下自推导（cargo-ndk 内部也是用这两个工具）
    if cc.is_none() || cxx.is_none() || ar.is_none() {
        if let Some(bin) = ndk_llvm_bin_dir() {
            cc = cc.or_else(|| existing_tool(&bin, "clang"));
            cxx = cxx.or_else(|| existing_tool(&bin, "clang++"));
            ar = ar.or_else(|| existing_tool(&bin, "llvm-ar"));
        }
    }

    let mut missing = Vec::new();
    if cc.is_none() {
        missing.push("CC");
    }
    if cxx.is_none() {
        missing.push("CXX");
    }
    if ar.is_none() {
        missing.push("AR");
    }
    if !missing.is_empty() {
        panic!("{}", android_toolchain_error(&rust_target, &missing));
    }
    let (Some(cc), Some(cxx), Some(ar)) = (cc, cxx, ar) else {
        unreachable!("missing 检查已保证三者齐全");
    };

    // cargo-ndk 的 CC 指向未带 target 的裸 clang，架构信息放在 CFLAGS 的
    // `--target=...` 里；自己从 NDK 推导时也要补上同样的 flag。
    let target_flag = format!("--target={clang_triple}{api}");
    let is_plain_clang = Path::new(&cc)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name == "clang");
    let is_x86 = matches!(arch.as_str(), "x86_64" | "x86");

    let mut asflags = None;
    if is_plain_clang {
        cflags = prepend_target_flag(cflags, &target_flag);
        cxxflags = prepend_target_flag(cxxflags, &target_flag);
        // ASFLAGS 只给 ARM：x86 的 AS 是 nasm/yasm，不认 clang 的 --target。
        if !is_x86 {
            asflags = Some(target_flag.clone());
        }
    }
    if is_x86 && !command_exists("nasm") && !command_exists("yasm") {
        // libvpx 的 x86 汇编写成 NASM 语法，clang 集成汇编器吃不了；NDK 也不带汇编器。
        panic!(
            "Android x86_64 / x86 目标需要 host 上的 NASM 或 YASM。\n\
             libvpx 的 x86 汇编使用 NASM 语法（NDK 只提供 clang/llvm，不含汇编器）。\n\
             安装示例：\n\
             \x20   macOS:          brew install nasm\n\
             \x20   Debian/Ubuntu:  sudo apt-get install nasm\n\
             \x20   Windows MSYS2:  pacman -S nasm"
        );
    }

    // 这些值会写进 config.mk 并由 make 交给 sh 执行；Windows 的反斜杠会被 sh
    // 当成转义吃掉（`C:\Android\...` → `C:Android...`），必须统一成正斜杠。
    let mut envs = vec![
        ("CC".to_string(), sh_path(&cc)),
        ("CXX".to_string(), sh_path(&cxx)),
        ("AR".to_string(), sh_path(&ar)),
        // libvpx 只在 configure 的链接探测里用 LD；与 CC 同源即可。
        ("LD".to_string(), sh_path(&cc)),
    ];
    if is_x86 {
        // 交给 configure 自己探测 nasm/yasm（上面的预检保证至少有一个）。
        envs.push(("AS".to_string(), "auto".to_string()));
    } else {
        // ARM：.S 汇编由 C 编译器驱动（NDK r24 起不再提供独立的 GNU as）。
        // 不显式设置时 setup_gnu_toolchain 会落到 host 的 `as`，必然失败。
        envs.push((
            "AS".to_string(),
            lookup_target_env("AS", &rust_target)
                .map_or_else(|| sh_path(&cc), |value| sh_path(&value)),
        ));
    }
    if let Some(value) = cflags {
        envs.push(("CFLAGS".to_string(), value));
    }
    if let Some(value) = cxxflags {
        envs.push(("CXXFLAGS".to_string(), value));
    }
    if let Some(value) = asflags {
        envs.push(("ASFLAGS".to_string(), value));
    }
    // libvpx 的链接探测走 `${LD} ${LDFLAGS}`，**不带 CFLAGS/CXXFLAGS**。若 CC 是
    // 裸 clang（cargo-ndk 的注入形态），target 只写在编译 flags 里，链接探测就会用
    // 主机默认目标去链 ARM 目标文件，报 `unknown file type`。故 LDFLAGS 也要补
    // 同一个 target flag；用 NDK 的 target 专用驱动（`aarch64-linux-android21-clang`）
    // 时不需要（驱动本身已绑定目标）。
    if is_plain_clang {
        envs.push(("LDFLAGS".to_string(), target_flag.clone()));
    }

    // bindgen 的 clang 参数：目标三元组 + NDK sysroot + clang 资源目录。
    let llvm_bin = ndk_llvm_bin_dir().or_else(|| {
        let parent = Path::new(&cc).parent()?;
        parent.is_dir().then(|| parent.to_path_buf())
    });
    let mut bindgen_args = vec![target_flag.clone()];
    let libclang_dir = llvm_bin.filter(|bin| {
        bin.join("libclang.dll").is_file()
            || bin.join("libclang.so").is_file()
            || bin.join("libclang.dylib").is_file()
    });
    if let Some(bin) = libclang_dir.as_ref() {
        if let Some(prebuilt) = bin.parent() {
            let sysroot = prebuilt.join("sysroot");
            if sysroot.is_dir() {
                bindgen_args.push(format!("--sysroot={}", sh_path(&sysroot.display().to_string())));
            }
            // `<prebuilt>/lib/clang/<version>/include/stddef.h`
            if let Ok(entries) = fs::read_dir(prebuilt.join("lib/clang")) {
                let resource_dir = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| path.join("include/stddef.h").is_file());
                if let Some(path) = resource_dir {
                    // clang 只认单横线的 `-resource-dir=`（双横线会报 unknown argument）。
                    bindgen_args.push(format!(
                        "-resource-dir={}",
                        sh_path(&path.display().to_string())
                    ));
                }
            }
        }
    }

    AndroidToolchain {
        libvpx_target,
        envs,
        bindgen_args,
        libclang_dir,
    }
}

/// 按 target triple 查找环境变量：`CC_<triple>` / `CC_<triple 下划线>` / `TARGET_CC`
fn lookup_target_env(base: &str, target: &str) -> Option<String> {
    for key in [
        format!("{base}_{target}"),
        format!("{base}_{}", target.replace('-', "_")),
        format!("TARGET_{base}"),
    ] {
        if let Ok(value) = env::var(&key) {
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// 把 Windows 路径转成 sh/make 能安全持有的形式（正斜杠）。
///
/// configure 会把 CC/CXX/AS 写进 `config.mk`，make 再通过 `sh -c` 执行；反斜杠在
/// sh 里是转义符，`C:\Android\...\clang.exe` 会被吃成 `C:Android...`。
fn sh_path(value: &str) -> String {
    if cfg!(windows) {
        value.replace('\\', "/")
    } else {
        value.to_string()
    }
}

/// CFLAGS 里还没有 `--target=` 时把目标 flag 加到最前面
fn prepend_target_flag(flags: Option<String>, target_flag: &str) -> Option<String> {
    match flags {
        Some(existing) if existing.contains("--target=") => Some(existing),
        Some(existing) => Some(format!("{target_flag} {existing}")),
        None => Some(target_flag.to_string()),
    }
}

/// NDK 根目录（cargo-ndk 与各 IDE 常用的几个变量都认）
fn ndk_home() -> Option<PathBuf> {
    [
        "ANDROID_NDK_HOME",
        "ANDROID_NDK_ROOT",
        "ANDROID_NDK",
        "NDK_HOME",
    ]
    .iter()
    .find_map(|key| {
        env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
    })
}

/// `<ndk>/toolchains/llvm/prebuilt/<host>/bin`（优先取真正存在 clang 的那份）
fn ndk_llvm_bin_dir() -> Option<PathBuf> {
    let prebuilt = ndk_home()?.join("toolchains/llvm/prebuilt");
    let mut dirs: Vec<PathBuf> = fs::read_dir(prebuilt)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin"))
        .filter(|bin| bin.join(exe_name("clang")).is_file())
        .collect();
    dirs.sort();
    dirs.into_iter().next()
}

/// bin 目录下存在的工具（返回绝对路径字符串）
fn existing_tool(bin: &Path, name: &str) -> Option<String> {
    let path = bin.join(exe_name(name));
    path.is_file().then(|| path.display().to_string())
}

/// Android API level（cargo-ndk 默认 21）
///
/// 只认 `ANDROID_PLATFORM` / `CARGO_NDK_PLATFORM`。**不能**用
/// `CARGO_NDK_ANDROID_PLATFORM`：cargo-ndk 实际把 ABI 名（`arm64-v8a`）写进了那个变量，
/// 取数字会得到 `64` 这种非法 API level。
fn android_api_level() -> String {
    for key in ["ANDROID_PLATFORM", "CARGO_NDK_PLATFORM"] {
        if let Ok(value) = env::var(key) {
            let digits: String = value.chars().filter(char::is_ascii_digit).collect();
            if let Ok(level) = digits.parse::<u32>() {
                if (1..=99).contains(&level) {
                    return level.to_string();
                }
            }
        }
    }
    "21".to_string()
}

/// host 上是否存在可执行的工具
fn command_exists(program: &str) -> bool {
    let works = |arg: &str| {
        Command::new(program)
            .arg(arg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    works("--version") || works("-v")
}

/// 缺少 Android 工具链时的可操作错误信息
fn android_toolchain_error(rust_target: &str, missing: &[&str]) -> String {
    let ndk = env::var("ANDROID_NDK_HOME").unwrap_or_else(|_| "<未设置>".to_string());
    format!(
        "Android 交叉编译缺少环境变量：{}。\n\
         \n\
         推荐直接用 cargo-ndk（它会按 target triple 注入 CC_/CXX_/AR_/CFLAGS_）：\n\
         \x20 rustup target add {rust_target}\n\
         \x20 cargo install cargo-ndk\n\
         \x20 cargo ndk -t arm64-v8a --platform 21 build -p rust-tunnel-remote\n\
         （模拟器把 -t 换成 x86_64；armv7 用 armeabi-v7a。完整命令见\n\
         \x20 scripts/build-android-libvpx.sh）\n\
         \n\
         也可以手工设置（变量名里的连字符是 target triple 原样）：\n\
         \x20 CC_{rust_target}  CXX_{rust_target}  AR_{rust_target}\n\
         \n\
         或者只设置 ANDROID_NDK_HOME（当前：{ndk}），build.rs 会从\n\
         <ndk>/toolchains/llvm/prebuilt/<host>/bin 推导 clang / clang++ / llvm-ar。\n\
         \n\
         注意：libvpx 1.16 的 configure 只认普通的 CC/CXX/AR，不认\n\
         CC_<libvpx-target>（`arm64-android-gcc` 带连字符，shell 无法展开）；\n\
         这层映射由 vendor/shiguredo_libvpx/build.rs 负责。",
        missing.join(" / ")
    )
}

// Unix 環境でのソースビルド
fn build_from_source_unix(src_dir: &Path) {
    let success = Command::new("./configure")
        .arg("--disable-shared")
        .arg("--enable-vp9-highbitdepth")
        .arg(format!("--prefix={}", src_dir.display()))
        .current_dir(src_dir)
        .status()
        .is_ok_and(|status| status.success());
    if !success {
        panic!("[configure] failed to build {LIB_NAME}");
    }

    let success = Command::new("make")
        .current_dir(src_dir)
        .status()
        .is_ok_and(|status| status.success());
    if !success {
        panic!("[make] failed to build {LIB_NAME}");
    }

    let success = Command::new("make")
        .arg("install")
        .current_dir(src_dir)
        .status()
        .is_ok_and(|status| status.success());
    if !success {
        panic!("[make install] failed to build {LIB_NAME}");
    }
}

// Windows + MSYS2 環境でのソースビルド
fn build_from_source_windows(src_dir: &Path) {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let configure_target = match target_arch.as_str() {
        "x86_64" => "x86_64-win64-gcc",
        _ => panic!("unsupported Windows arch for source-build: {}", target_arch),
    };

    // `configure` はシェルスクリプトなので sh 経由で実行する
    run_with_shell(
        src_dir,
        &format!(
            "./configure --target={configure_target} \
             --disable-shared --enable-vp9-highbitdepth --prefix=\"$(pwd)\""
        ),
        "configure",
    );
    run_with_shell(src_dir, "make", "make");
    run_with_shell(src_dir, "make install", "make install");
}

// shell 経由でコマンドを実行する
fn run_with_shell(src_dir: &Path, command: &str, step_name: &str) {
    let success = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(src_dir)
        .status()
        .is_ok_and(|status| status.success());
    if !success {
        panic!("[{}] failed to build {LIB_NAME} on Windows", step_name);
    }
}

// --- シンボル書き換え ---
//
// 他のライブラリとのシンボル衝突を回避するため、静的ライブラリ内の全シンボルに
// プレフィックスを付与する仕組み。
//
// llvm-nm / llvm-objcopy は rustup の llvm-tools コンポーネントに含まれるものを使用する。
// rust-toolchain.toml に components = ["llvm-tools"] の記載が必要。
//
// プラットフォームごとのシンボル形式の違い:
//   - macOS (Mach-O): シンボル先頭に `_` が付く (例: _vpx_codec_encode)
//   - Linux (ELF): 先頭 `_` なし (例: vpx_codec_encode)
//   - Windows x64 (COFF): 先頭 `_` なし (例: vpx_codec_encode)
//
// bindgen の generated_link_name_override は返した文字列に \u{1} プレフィックスを
// 自動付加する。\u{1} はコンパイラに「この名前をそのまま使え（マングリングするな）」と
// 指示するため、プラットフォーム固有のシンボル名（macOS なら _shiguredo_vpx_codec_encode）を
// そのまま返す必要がある。

/// llvm-nm / llvm-objcopy のパスを保持する
struct LlvmTools {
    nm: PathBuf,
    objcopy: PathBuf,
}

/// objcopy 用と bindgen 用の 2 つのリネームマップを保持する
///
/// 2 つのマップが必要な理由:
///   - objcopy_map: ライブラリ内の実シンボル名を書き換えるため、プラットフォーム依存の名前を使う
///   - bindgen_map: Rust コードからリンクする際の名前を指定するため、C シンボル名をキーにする
struct SymbolRenameMaps {
    /// llvm-objcopy の --redefine-syms 用マップ
    ///
    /// キー: 元のシンボル名 (例: macOS なら _vpx_codec_encode、Linux なら vpx_codec_encode)
    /// 値: 書き換え後のシンボル名 (例: macOS なら _shiguredo_vpx_codec_encode)
    objcopy_map: HashMap<String, String>,

    /// bindgen の #[link_name] 用マップ
    ///
    /// キー: C シンボル名 (プラットフォーム非依存、例: vpx_codec_encode)
    /// 値: 書き換え後のシンボル名 (プラットフォーム依存、例: macOS なら _shiguredo_vpx_codec_encode)
    ///
    /// bindgen は \u{1} プレフィックスを付加してマングリングを抑制するため、
    /// 値にはプラットフォーム固有のシンボル名を格納する必要がある。
    bindgen_map: HashMap<String, String>,
}

/// bindgen の ParseCallbacks 実装
///
/// バインディング生成時に、書き換え後のシンボル名を `#[link_name = "..."]` として付与する。
/// これにより lib.rs 側のコード変更なしでシンボル書き換えが透過的に動作する。
#[derive(Debug)]
struct SymbolLinkNameCallbacks {
    /// C シンボル名 → 書き換え後シンボル名のマップ
    rename_map: HashMap<String, String>,
}

impl bindgen::callbacks::ParseCallbacks for SymbolLinkNameCallbacks {
    /// bindgen がバインディングを生成する際に呼ばれるコールバック
    ///
    /// 戻り値が Some の場合、bindgen は #[link_name = "\u{1}<戻り値>"] を生成する。
    /// \u{1} プレフィックスによりコンパイラのシンボルマングリングが抑制されるため、
    /// 戻り値にはプラットフォーム固有のシンボル名を返す必要がある。
    fn generated_link_name_override(
        &self,
        item_info: bindgen::callbacks::ItemInfo<'_>,
    ) -> Option<String> {
        self.rename_map.get(item_info.name).cloned()
    }
}

/// 静的ライブラリのシンボルを書き換え、bindgen 用の ParseCallbacks を返す
///
/// 処理の流れ:
///   1. rustup の sysroot から llvm-nm / llvm-objcopy を探す
///   2. llvm-nm で静的ライブラリの定義済み外部シンボルを収集する
///   3. 収集したシンボルに対してリネームマップを生成する
///   4. マップファイルを書き出し、llvm-objcopy でライブラリ内のシンボルを書き換える
///   5. bindgen 用の ParseCallbacks を返す
fn rewrite_symbols(lib_dir: &Path, out_dir: &Path) -> SymbolLinkNameCallbacks {
    let tools = discover_llvm_tools();
    let lib_path = find_static_library(lib_dir);

    // macOS の Mach-O ではシンボル先頭に `_` が付くため、
    // プラットフォーム判定してリネームマップの生成時に考慮する
    let is_macos = env::var("CARGO_CFG_TARGET_OS")
        .map(|v| v == "macos")
        .unwrap_or(false);

    // シンボル名の変換ルール
    //
    // vpx_ プレフィックスを持つシンボル (公開 API) は vpx_ を SYMBOL_PREFIX_ に置換する。
    //   例: vpx_codec_encode → shiguredo_vpx_codec_encode
    //
    // それ以外のシンボル (vp8_*, vp9_* 等の内部シンボル) は先頭に SYMBOL_PREFIX_ を付与する。
    //   例: vp8_denoiser_free → shiguredo_vpx_vp8_denoiser_free
    let rename_symbol = |name: &str| -> Option<String> {
        if let Some(rest) = name.strip_prefix("vpx_") {
            Some(format!("{SYMBOL_PREFIX}_{rest}"))
        } else {
            Some(format!("{SYMBOL_PREFIX}_{name}"))
        }
    };

    // 全定義済み外部シンボルを収集してリネームマップを生成する
    let symbols = collect_defined_external_symbols(&tools.nm, &lib_path);
    let maps = build_symbol_rename_maps(&symbols, is_macos, &rename_symbol);

    // マップファイルを書き出してシンボルを書き換える
    let map_file = out_dir.join("symbol_rename_map.txt");
    write_objcopy_rename_map(&maps.objcopy_map, &map_file);
    rewrite_archive_symbols(&tools.objcopy, &lib_path, &map_file);

    SymbolLinkNameCallbacks {
        rename_map: maps.bindgen_map,
    }
}

/// 静的ライブラリのパスを探す
///
/// ビルド結果は Unix 系では libvpx.a、Windows では vpx.lib として出力される。
fn find_static_library(lib_dir: &Path) -> PathBuf {
    let unix_path = lib_dir.join("libvpx.a");
    if unix_path.exists() {
        return unix_path;
    }
    let win_path = lib_dir.join("vpx.lib");
    if win_path.exists() {
        return win_path;
    }
    panic!("static library not found in {}", lib_dir.display());
}

/// rustc --print sysroot の結果を取得する
///
/// llvm-tools は rustup が管理する sysroot 配下にインストールされるため、
/// sysroot のパスを取得して llvm-nm / llvm-objcopy の探索に使用する。
fn get_rustc_sysroot() -> PathBuf {
    let output = Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .expect("failed to run rustc --print sysroot");
    if !output.status.success() {
        panic!("rustc --print sysroot failed");
    }
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("invalid UTF-8")
            .trim(),
    )
}

/// Windows 対応の実行ファイル名を生成する
///
/// Windows では実行ファイルに .exe 拡張子が必要。
fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// rustup の sysroot から llvm-nm / llvm-objcopy を探す
///
/// llvm-tools コンポーネントのバイナリは以下のパスに配置される:
///   <sysroot>/lib/rustlib/<host>/bin/llvm-nm
///   <sysroot>/lib/rustlib/<host>/bin/llvm-objcopy
///
/// rust-toolchain.toml に llvm-tools コンポーネントの記載が必要。
///
/// llvm-nm / llvm-objcopy はホスト上で実行するツールなので、クロスコンパイル時は
/// TARGET ではなく HOST のパスから探す必要がある。
/// 例: Windows CI でホストが x86_64-pc-windows-msvc、ターゲットが x86_64-pc-windows-gnu の場合、
/// llvm-tools は msvc 側にのみインストールされている。
fn discover_llvm_tools() -> LlvmTools {
    let sysroot = get_rustc_sysroot();
    // llvm-tools はホスト上で動作するため HOST を使う。
    // クロスコンパイル時に TARGET を使うと、ホスト側にインストールされた
    // llvm-tools が見つからない。
    let host = env::var("HOST").expect("HOST environment variable not set");
    let tools_dir = sysroot.join("lib/rustlib").join(host).join("bin");

    let nm = tools_dir.join(exe_name("llvm-nm"));
    let objcopy = tools_dir.join(exe_name("llvm-objcopy"));

    if !nm.exists() {
        panic!(
            "llvm-nm not found at {}. Run: rustup component add llvm-tools",
            nm.display()
        );
    }
    if !objcopy.exists() {
        panic!(
            "llvm-objcopy not found at {}. Run: rustup component add llvm-tools",
            objcopy.display()
        );
    }

    LlvmTools { nm, objcopy }
}

/// llvm-nm で静的ライブラリから定義済み外部シンボルを収集する
///
/// llvm-nm のオプション:
///   --defined-only: 定義済みシンボルのみ (未定義シンボルを除外)
///   --extern-only: 外部シンボルのみ (ローカルシンボルを除外)
///   --format=just-symbols: シンボル名のみ出力 (アドレスやタイプを省略)
///
/// 出力にはオブジェクトファイル名 (例: vp8_cx_iface.c.o:) も含まれるため、
/// is_c_identifier() でフィルタリングして純粋なシンボル名のみを抽出する。
fn collect_defined_external_symbols(nm_path: &Path, lib_path: &Path) -> Vec<String> {
    let output = Command::new(nm_path)
        .arg("--defined-only")
        .arg("--extern-only")
        .arg("--format=just-symbols")
        .arg(lib_path)
        .output()
        .expect("failed to run llvm-nm");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("llvm-nm failed: {stderr}");
    }

    let stdout = String::from_utf8(output.stdout).expect("llvm-nm output is not valid UTF-8");
    let mut symbols: Vec<String> = stdout
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|s| !s.is_empty() && is_c_identifier(s))
        .collect();
    symbols.sort();
    symbols.dedup();
    symbols
}

/// C 識別子として有効かどうかを判定する
///
/// llvm-nm の --format=just-symbols 出力にはオブジェクトファイル名 (vp8_cx_iface.c.o: 等) も
/// 含まれるため、この関数で C 識別子のみをフィルタリングする。
///
/// macOS の Mach-O ではシンボル先頭に `_` が付くため、`_` で始まる文字列も受け入れる。
fn is_c_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// objcopy 用と bindgen 用のリネームマップを生成する
///
/// 2 つのマップを生成する理由:
///
/// objcopy_map: ライブラリバイナリ内の実シンボル名を書き換えるためのマップ。
///   macOS では _vpx_codec_encode → _shiguredo_vpx_codec_encode のようにプラットフォーム固有の
///   `_` プレフィックスを含む形で管理する。
///
/// bindgen_map: Rust バインディングの #[link_name] に使うマップ。
///   キーは C シンボル名 (vpx_codec_encode)、値はプラットフォーム固有のシンボル名
///   (_shiguredo_vpx_codec_encode) を格納する。
///   bindgen は generated_link_name_override の戻り値に \u{1} を付加してマングリングを
///   抑制するため、プラットフォーム固有の名前を直接返す必要がある。
fn build_symbol_rename_maps(
    symbols: &[String],
    is_macos: bool,
    rename_symbol: &dyn Fn(&str) -> Option<String>,
) -> SymbolRenameMaps {
    let mut objcopy_map = HashMap::new();
    let mut bindgen_map = HashMap::new();

    for sym in symbols {
        // プラットフォーム固有のプレフィックスを除去して C シンボル名を取得する
        //   macOS: _vpx_codec_encode → vpx_codec_encode
        //   Linux/Windows: vpx_codec_encode → vpx_codec_encode (変化なし)
        let c_name = if is_macos {
            sym.strip_prefix('_').unwrap_or(sym)
        } else {
            sym.as_str()
        };

        if let Some(new_c_name) = rename_symbol(c_name) {
            // objcopy 用: プラットフォーム固有のプレフィックスを再付与する
            //   macOS: shiguredo_vpx_codec_encode → _shiguredo_vpx_codec_encode
            //   Linux/Windows: shiguredo_vpx_codec_encode → shiguredo_vpx_codec_encode (変化なし)
            let new_sym = if is_macos {
                format!("_{new_c_name}")
            } else {
                new_c_name.clone()
            };
            objcopy_map.insert(sym.clone(), new_sym.clone());

            // bindgen 用: generated_link_name_override は \u{1} プレフィックスを付加して
            // シンボル名をそのまま使うため、プラットフォーム固有のシンボル名で管理する
            bindgen_map.insert(c_name.to_string(), new_sym);
        }
    }

    SymbolRenameMaps {
        objcopy_map,
        bindgen_map,
    }
}

/// --redefine-syms 用のマップファイルを書き出す
///
/// ファイル形式は 1 行に "旧シンボル名 新シンボル名" を空白区切りで記述する。
/// llvm-objcopy の --redefine-syms オプションで使用される。
fn write_objcopy_rename_map(map: &HashMap<String, String>, path: &Path) {
    let mut lines: Vec<String> = map
        .iter()
        .map(|(old, new)| format!("{old} {new}"))
        .collect();
    // 出力を決定的にするためソートする
    lines.sort();
    fs::write(path, lines.join("\n")).expect("failed to write symbol rename map");
}

/// llvm-objcopy でアーカイブ内のシンボルを書き換える
///
/// --redefine-syms はマップファイルに従ってシンボル名を一括置換する。
/// ライブラリファイルはインプレースで更新される。
fn rewrite_archive_symbols(objcopy_path: &Path, lib_path: &Path, map_file: &Path) {
    let status = Command::new(objcopy_path)
        .arg("--redefine-syms")
        .arg(map_file)
        .arg(lib_path)
        .status()
        .expect("failed to run llvm-objcopy");
    if !status.success() {
        panic!("llvm-objcopy failed");
    }
}

// --- 既存のヘルパー関数 ---

// CARGO_CFG_TARGET_OS + CARGO_CFG_TARGET_ARCH からプラットフォーム名を生成する
fn get_target_platform() -> String {
    if let Ok(target) = env::var("LIBVPX_TARGET") {
        return target;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    match (target_os.as_str(), target_arch.as_str()) {
        ("linux", "x86_64") => format!("{}_x86_64", detect_linux_distro()),
        ("linux", "aarch64") => format!("{}_arm64", detect_linux_distro()),
        ("macos", "aarch64") => "macos_arm64".to_string(),
        ("windows", "x86_64") => "windows_x86_64".to_string(),
        _ => panic!("unsupported target: os={}, arch={}", target_os, target_arch),
    }
}

// /etc/os-release から Ubuntu バージョンを検出する
fn detect_linux_distro() -> String {
    if let Ok(content) = fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if let Some(version) = line.strip_prefix("VERSION_ID=") {
                let version = version.trim_matches('"');
                match version {
                    "22.04" | "24.04" => return format!("ubuntu-{}", version),
                    _ => {}
                }
            }
        }
    }
    panic!(
        "unsupported Linux distribution. \
         set LIBVPX_TARGET environment variable to specify the target explicitly"
    );
}

// 外部ライブラリのリポジトリを git clone する
fn git_clone_external_lib(build_dir: &Path) {
    let (url, version) = get_url_and_version();
    let success = Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--branch")
        .arg(version)
        .arg(url)
        .current_dir(build_dir)
        .status()
        .is_ok_and(|status| status.success());
    if !success {
        panic!("failed to clone {LIB_NAME} repository");
    }
}

// Cargo.toml から依存ライブラリの URL とバージョンタグを取得する
fn get_url_and_version() -> (String, String) {
    let cargo_toml = shiguredo_toml::Value::Table(
        shiguredo_toml::from_str(include_str!("Cargo.toml")).expect("failed to parse Cargo.toml"),
    );
    if let Some((Some(url), Some(version))) = cargo_toml
        .get("package")
        .and_then(|v| v.get("metadata"))
        .and_then(|v| v.get("external-dependencies"))
        .and_then(|v| v.get(LIB_NAME))
        .map(|v| {
            (
                v.get("url").and_then(|s| s.as_str()),
                v.get("version").and_then(|s| s.as_str()),
            )
        })
    {
        (url.to_string(), version.to_string())
    } else {
        panic!(
            "Cargo.toml does not contain a valid [package.metadata.external-dependencies.{LIB_NAME}] table"
        );
    }
}
