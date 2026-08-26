//! comrak 0.54 API 探针（临时测试模块，验证后并入 markdown.rs / link.rs 或删除）。
//!
//! 只运行一次：`cargo test -p rust-tunnel-wiki-core --lib comrak_probe`
//!
//! # 五个问题的结论（comrak 0.54.0，2026-08-26 实测）
//!
//! 1. **`[[a|b]]` 的 AST 形状**（`wikilinks_title_after_pipe: true`，即 UrlFirst 模式）：
//!    `NodeValue::WikiLink { url: "a" }` —— 管道**前**是 url（href 目标）；
//!    显示标题是管道**后**的 `b`，落在 WikiLink 的**子 Text 节点**里
//!    （`NodeValue::Text("b")`）。无管道的 `[[b]]` 则 url 与子 Text 都是 `b`。
//!
//! 2. **`![[x]]`（Obsidian 嵌入）在 comrak 0.54 不产生 WikiLink，也不产生 Image**：
//!    整段退化为普通文本（`![[x]]` 原样输出）。根因：`!` 处理器先消费 `![`、
//!    把 `within_brackets` 置 true 并 push 一个 image 括号；随后 `[` 命中 wikilink
//!    分支的条件 `!self.within_brackets` 不成立，wikilink 解析被跳过。
//!    → 解析器必须**自己处理** `![[...]]` 嵌入语法，comrak 不会帮忙。
//!
//! 3. **围栏代码块内的 `[[a]]` 不产生 WikiLink**：代码块内容按 raw literal 处理，
//!    只出现 `NodeValue::CodeBlock`，其 `literal` 原样包含 `[[a]]`。
//!
//! 4. **Front Matter 边界**：`NodeValue::FrontMatter(s)` 的 `s` **包含首尾 `---` 分隔符本身**，
//!    实测 `"---\ntitle: hello\n---\n\n"`（含关闭分隔符后的空行）。后续若要交给
//!    gray_matter / serde_yaml 解析或自绘，须自行剥掉分隔符。
//!
//! 5. **`ExtensionOptions` / `ParseOptions` 路径**：**这两个类型在 comrak 0.54 中不存在**，
//!    `use comrak::ExtensionOptions;` 直接编译失败。正确路径：
//!    - 伞结构 `comrak::Options`（真实定义 `comrak::parser::options::Options`）；
//!    - `Options::extension` 的类型是 `comrak::options::Extension<'c>`；
//!    - `Options::parse` 的类型是 `comrak::options::Parse<'c>`；
//!    - `parser` 模块本身是私有的，crate 根 `pub use parser::options;` 把 `options`
//!      模块再导出为公开的 `comrak::options`；
//!    - 字段位置：`wikilinks_title_after_pipe` 与 `front_matter_delimiter` 都挂在
//!      **`extension`** 上（0.54 把 front_matter_delimiter 放在 extension，不在 parse）。
//!
//! ## 旧 API 迁移提示（给 markdown.rs / link.rs 开发者）
//! 旧 comrak（≤0.27 左右）的 `ExtensionOptions` / `ParseOptions` 对应 0.54 的
//! `comrak::options::Extension` / `comrak::options::Parse`；旧
//! `ParseOptions::front_matter_delimiter` 在 0.54 移到 `options.extension.front_matter_delimiter`。

use comrak::{nodes::NodeValue, parse_document, Arena, Options};

/// 遍历 `node` 的子节点，收集所有 Text 节点的内容（wikilink 的显示标题在子 Text 里）。
fn child_texts(node: comrak::nodes::Node<'_>) -> Vec<String> {
    node.children()
        .filter_map(|child| {
            let ast = child.data();
            match &ast.value {
                NodeValue::Text(t) => Some(t.to_string()),
                _ => None,
            }
        })
        .collect()
}

/// 把 AST 递归打印成缩进树，便于肉眼核对节点类型。
fn dump(node: comrak::nodes::Node<'_>, depth: usize) -> String {
    let ast = node.data();
    let label = match &ast.value {
        NodeValue::Text(t) => format!("Text({t:?})"),
        NodeValue::WikiLink(l) => format!("WikiLink(url={:?})", l.url),
        NodeValue::Image(..) => "Image(..)".to_string(),
        NodeValue::CodeBlock(cb) => format!("CodeBlock(literal={:?})", cb.literal),
        NodeValue::FrontMatter(s) => format!("FrontMatter({s:?})"),
        other => other.xml_node_name().to_string(),
    };
    let mut out = format!("{}{label}\n", "  ".repeat(depth));
    for child in node.children() {
        out.push_str(&dump(child, depth + 1));
    }
    out
}

/// 问题 1：`[[a|b]]`（after_pipe）的 AST 形状 —— url 与子 Text 各是哪个。
#[test]
fn wikilink_pipe_ast_shape() {
    let mut options = Options::default();
    options.extension.wikilinks_title_after_pipe = true;

    let arena = Arena::new();
    let root = parse_document(&arena, "[[a|b]]", &options);

    let mut wikilinks: Vec<(String, Vec<String>)> = Vec::new();
    for node in root.descendants() {
        let ast = node.data();
        if let NodeValue::WikiLink(link) = &ast.value {
            println!("wikilink node: url = {:?}", link.url);
            println!("children = {:#?}", child_texts(node));
            wikilinks.push((link.url.clone(), child_texts(node)));
        }
    }
    println!("AST 全貌:\n{}", dump(root, 0));
    println!(
        "=> `[[a|b]]` (after_pipe) 结论: url={:?}, 子 Text={:?}",
        wikilinks[0].0, wikilinks[0].1
    );

    assert_eq!(wikilinks.len(), 1, "应恰好产生 1 个 WikiLink 节点");
    assert_eq!(wikilinks[0].0, "a", "url 应为管道前的 'a'");
    assert_eq!(
        wikilinks[0]
            .1
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["b"],
        "子 Text 应为管道后的 'b'（显示标题）"
    );
}

/// 问题 2：`![[x]]`（Obsidian 嵌入）产生什么节点。
#[test]
fn obsidian_embed_node_type() {
    let mut options = Options::default();
    options.extension.wikilinks_title_after_pipe = true;

    let arena = Arena::new();
    let root = parse_document(&arena, "![[x]]", &options);

    let mut wikilink_count = 0usize;
    let mut image_count = 0usize;
    for node in root.descendants() {
        let ast = node.data();
        match &ast.value {
            NodeValue::WikiLink(..) => wikilink_count += 1,
            NodeValue::Image(..) => image_count += 1,
            _ => {}
        }
    }
    println!("AST 全貌:\n{}", dump(root, 0));
    println!(
        "=> `![[x]]` 结论: wikilink 数={wikilink_count}, image 数={image_count}（期望均为 0，整段退化为普通文本）"
    );

    assert_eq!(wikilink_count, 0, "`![[x]]` 不应产生 WikiLink 节点");
    assert_eq!(image_count, 0, "`![[x]]` 不应产生 Image 节点");
}

/// 问题 3：围栏代码块内的 `[[a]]` 不产生 WikiLink。
#[test]
fn fenced_code_no_wikilink() {
    let mut options = Options::default();
    options.extension.wikilinks_title_after_pipe = true;

    let arena = Arena::new();
    let root = parse_document(&arena, "```\n[[a]]\n```", &options);

    let mut has_code_block = false;
    let mut code_literal = String::new();
    let mut wikilink_count = 0usize;
    for node in root.descendants() {
        let ast = node.data();
        match &ast.value {
            NodeValue::WikiLink(..) => wikilink_count += 1,
            NodeValue::CodeBlock(cb) => {
                has_code_block = true;
                code_literal = cb.literal.clone();
            }
            _ => {}
        }
    }
    println!("AST 全貌:\n{}", dump(root, 0));
    println!(
        "=> 围栏代码块结论: has_code_block={has_code_block}, literal={code_literal:?}, wikilink 数={wikilink_count}"
    );

    assert!(has_code_block, "围栏代码块应产生 CodeBlock 节点");
    assert!(
        code_literal.contains("[[a]]"),
        "代码块 literal 应原样含 `[[a]]`"
    );
    assert_eq!(wikilink_count, 0, "代码块内 `[[a]]` 不应产生 WikiLink");
}

/// 问题 4：Front Matter 边界 —— `FrontMatter(s)` 的 `s` 是否含 `---` 分隔符本身。
#[test]
fn front_matter_delimiter_included() {
    let mut options = Options::default();
    options.extension.front_matter_delimiter = Some("---".to_owned());

    let arena = Arena::new();
    let input = "---\ntitle: hello\n---\n\n正文";
    let root = parse_document(&arena, input, &options);

    let mut front: Option<String> = None;
    for node in root.descendants() {
        let ast = node.data();
        if let NodeValue::FrontMatter(s) = &ast.value {
            println!("front matter 节点内容 = {s:?}");
            front = Some(s.clone());
        }
    }
    let s = front.expect("应产生 FrontMatter 节点");
    println!(
        "=> FrontMatter 结论: 内容含 `---` 分隔符={}, 完整内容={s:?}",
        s.contains("---")
    );

    assert!(
        s.contains("---"),
        "FrontMatter 字符串应包含 `---` 分隔符本身"
    );
    assert!(s.starts_with("---\n"), "应以 `---\\n` 开头");
    assert!(s.contains("\ntitle: hello\n"), "应包含 front matter 正文");
}

/// 问题 5：`ExtensionOptions` / `ParseOptions` 在 0.54 的完整路径。
#[test]
fn options_type_paths_0_54() {
    // 旧类型名在 0.54 已不存在：`use comrak::ExtensionOptions;` 会编译失败
    // （no `ExtensionOptions` in `comrak`）。下面用正确公开路径编译，
    // 并用运行时 type_name 打印完整类型路径作为可执行证据。
    use comrak::options::{Extension, Parse};
    use std::any::type_name;

    let mut options: Options<'_> = Options::default();

    // 编译期验证：`Options` 的 extension / parse 字段类型是 `comrak::options::Extension` / `Parse`。
    let ext: &Extension<'_> = &options.extension;
    let parse: &Parse<'_> = &options.parse;
    let _ = (ext, parse); // 仅作类型检查占位。

    // 运行时完整路径（真实定义模块是 comrak::parser::options，crate 根再导出为 comrak::options）。
    println!(
        "Options 的完整类型路径: {}",
        type_name::<Options<'static>>()
    );
    println!(
        "Extension 的完整类型路径: {}",
        type_name::<Extension<'static>>()
    );
    println!("Parse 的完整类型路径: {}", type_name::<Parse<'static>>());

    // 字段位置：wikilinks 与 front_matter_delimiter 都在 `extension` 上（0.54）。
    options.extension.wikilinks_title_after_pipe = true;
    options.extension.front_matter_delimiter = Some("---".to_owned());
    options.parse.smart = true;

    println!("=> 结论: 0.54 无 ExtensionOptions/ParseOptions; 正确路径为 comrak::options::Extension / comrak::options::Parse（经 comrak::Options.extension / .parse 访问）");

    assert!(options.extension.wikilinks_title_after_pipe);
    assert_eq!(
        options.extension.front_matter_delimiter.as_deref(),
        Some("---")
    );
    assert!(options.parse.smart);
}
