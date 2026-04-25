# Rust Demo 基础项目

一个包含 Rust 基础语法示例的项目。

## 项目结构

```
.
├── Cargo.toml      # 项目配置
└── src/
    └── main.rs     # 主程序，包含多个基础示例
```

## 安装 Rust

如果你还没有安装 Rust，可以通过以下方式安装：

**macOS / Linux:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

安装完成后需要重新加载终端环境：
```bash
source $HOME/.cargo/env
```

**Windows:**
下载并运行 [rustup-init.exe](https://rustup.rs/)

## 运行项目

安装完成后，在项目目录下运行：

```bash
# 直接运行
cargo run

# 编译项目，生成可执行文件在 target/debug/
cargo build

# 检查代码错误，不生成可执行文件
cargo check
```

## 示例内容

`main.rs` 中包含以下 Rust 基础概念：

1. 变量和可变性 (`let` vs `let mut`)
2. 基本数据类型 (布尔、整数、浮点数)
3. 元组和元组解构
4. 数组
5. 函数定义和调用
6. 条件表达式 `if/else`
7. `for` 循环
8. `Vec` 动态数组
9. `match` 模式匹配
10. 结构体 `struct` 和方法 `impl`
11. 枚举 `enum`
