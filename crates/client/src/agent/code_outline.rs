//! Code structure analysis tools using tree-sitter.
//! Provides `code_outline` (file structure overview) and `read_symbol` (extract named symbol).

use rust_tunnel_common::AgentResult;

/// 最大返回符号数（超出截断）
const MAX_OUTLINE_SYMBOLS: usize = 200;

/// 单条输出上限（复用 MAX_OUTPUT 约束）
const MAX_OUTPUT: usize = 100 * 1024;

/// 按扩展名选择 tree-sitter Language
fn language_for_ext(ext: &str) -> Result<tree_sitter::Language, String> {
    match ext.to_lowercase().as_str() {
        "rs" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "py" => Ok(tree_sitter_python::LANGUAGE.into()),
        "ts" | "tsx" => Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "js" | "jsx" | "mjs" | "cjs" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "go" => Ok(tree_sitter_go::LANGUAGE.into()),
        other => Err(format!(
            "code_outline does not support .{other} files; use read_file instead"
        )),
    }
}

/// 符号条目
struct SymbolEntry {
    kind: String,
    name: String,
    start_line: u64,
    end_line: u64,
    indent: usize,
}

/// 递归收集符号（一级嵌套：类/impl/trait 内的方法缩进）
fn collect_symbols(node: &tree_sitter::Node, source: &[u8], depth: usize) -> Vec<SymbolEntry> {
    let mut symbols = Vec::new();
    // 语言无关的符号 kind 集合（function/method/struct/class/impl/trait/interface/enum/module/type）
    let is_symbol_kind = |kind: &str| {
        matches!(
            kind,
            "function_definition"
                | "function_item"
                | "function_declaration"
                | "method_definition"
                | "method_declaration"
                | "struct_item"
                | "class_declaration"
                | "class_definition"
                | "impl_item"
                | "trait_item"
                | "interface_declaration"
                | "enum_item"
                | "enum_declaration"
                | "module"
                | "mod_item"
                | "type_item"
                | "type_alias_declaration"
                | "declaration"
                | "function_signature"
                | "abstract_method_declaration"
        )
    };

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if is_symbol_kind(kind) {
            let name = extract_symbol_name(child, source);
            let start_line = child.start_position().row as u64 + 1;
            let end_line = child.end_position().row as u64 + 1;
            symbols.push(SymbolEntry {
                kind: kind.to_string(),
                name,
                start_line,
                end_line,
                indent: depth,
            });
            // 一级嵌套：收集子符号（方法等）
            // 对 impl/trait/class 等复合类型，需要进入 body 声明列表查找嵌套方法
            if depth == 0 {
                let body_children = find_body_children(child);
                for sub in body_children {
                    let sub_kind = sub.kind();
                    if is_symbol_kind(sub_kind) {
                        let sub_name = extract_symbol_name(sub, source);
                        symbols.push(SymbolEntry {
                            kind: sub_kind.to_string(),
                            name: sub_name,
                            start_line: sub.start_position().row as u64 + 1,
                            end_line: sub.end_position().row as u64 + 1,
                            indent: 1,
                        });
                    }
                }
            }
        }
    }
    symbols
}

/// 对于复合类型（impl/trait/class 等），返回其 body 内的直接子节点。
/// tree-sitter-rust 的 impl_item 结构为 `(impl_item type_identifier (declaration_list ...))`，
/// 方法在 declaration_list 内而非 impl_item 的直接子节点。此函数找到 body 节点并返回
/// 其 named_children，以便收集嵌套符号。非复合类型返回空（不需嵌套扫描）。
fn find_body_children(node: tree_sitter::Node) -> Vec<tree_sitter::Node> {
    // body 字段（class_definition 等）或 declaration_list（rust impl/trait）
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        return body.named_children(&mut cursor).collect();
    }
    // 遍历直接子节点寻找 declaration_list / class_body / interface_body 等容器
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "declaration_list" | "class_body" | "interface_body" | "module_body" => {
                let mut sub_cursor = child.walk();
                return child.named_children(&mut sub_cursor).collect();
            }
            _ => {}
        }
    }
    // 非复合类型：直接返回子节点（如 function_item 的参数/块，不影响外部）
    Vec::new()
}

/// 从语法节点提取符号名称：优先 `name` 字段，回退到 `type` 字段（impl_item 用），
/// 再回退到节点文本首 token。
fn extract_symbol_name(node: tree_sitter::Node, source: &[u8]) -> String {
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(s) = n.utf8_text(source) {
            return s.to_string();
        }
    }
    // impl_item / class 等：`type` 字段（tree-sitter-rust impl_item）
    if let Some(t) = node.child_by_field_name("type") {
        if let Ok(s) = t.utf8_text(source) {
            return s.to_string();
        }
    }
    // 最终回退：取节点文本到第一个换行/空格/`{`/`(` 为止
    if let Ok(text) = node.utf8_text(source) {
        let end = text
            .find(['{', '(', '\n', ' '])
            .unwrap_or(text.len());
        let candidate = text[..end].trim();
        if !candidate.is_empty() {
            return candidate.to_string();
        }
    }
    "<anonymous>".to_string()
}

/// 格式化 kind 为简短标签
fn kind_label(kind: &str) -> &str {
    match kind {
        "function_definition" | "function_item" | "function_declaration" | "function_signature" => "fn",
        "method_definition" | "method_declaration" | "abstract_method_declaration" => "fn",
        "struct_item" | "class_declaration" | "class_definition" => "struct",
        "impl_item" => "impl",
        "trait_item" | "interface_declaration" => "trait",
        "enum_item" | "enum_declaration" => "enum",
        "module" | "mod_item" => "mod",
        "type_item" | "type_alias_declaration" => "type",
        _ => kind,
    }
}

/// 执行 code_outline：解析文件并输出符号列表
pub fn exec_outline(content: &str, path: &str) -> AgentResult {
    let ext = path.rsplit('.').next().unwrap_or("");
    let lang = match language_for_ext(ext) {
        Ok(l) => l,
        Err(e) => return AgentResult::Error { message: e },
    };
    let total_lines = content.lines().count();
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return AgentResult::Error {
            message: format!("failed to set language for .{ext}"),
        };
    }
    let tree = parser.parse(content.as_bytes(), None).unwrap();
    let root = tree.root_node();
    let symbols = collect_symbols(&root, content.as_bytes(), 0);

    let truncated = symbols.len() > MAX_OUTLINE_SYMBOLS;
    let display_symbols = if truncated {
        &symbols[..MAX_OUTLINE_SYMBOLS]
    } else {
        &symbols
    };

    let mut output = format!("{path} ({total_lines} lines, {ext})\n");
    for sym in display_symbols {
        let indent = "  ".repeat(sym.indent);
        let label = kind_label(&sym.kind);
        output.push_str(&format!(
            "{indent}[{label}] {} \u{2014} lines {}-{}\n",
            sym.name, sym.start_line, sym.end_line
        ));
    }
    if truncated {
        output.push_str(&format!(
            "[truncated at {} symbols, total {}]\n",
            MAX_OUTLINE_SYMBOLS,
            symbols.len()
        ));
    }
    AgentResult::FileContent {
        content: truncate_output_str(&output),
    }
}

/// 执行 read_symbol：按名称精确匹配符号并返回源码
pub fn exec_read_symbol(content: &str, path: &str, name: &str) -> AgentResult {
    let ext = path.rsplit('.').next().unwrap_or("");
    let lang = match language_for_ext(ext) {
        Ok(l) => l,
        Err(e) => return AgentResult::Error { message: e },
    };
    let total_lines = content.lines().count();
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return AgentResult::Error {
            message: format!("failed to set language for .{ext}"),
        };
    }
    let tree = parser.parse(content.as_bytes(), None).unwrap();
    let root = tree.root_node();
    let symbols = collect_symbols(&root, content.as_bytes(), 0);

    // 按名称匹配（大小写敏感精确匹配）
    let matches: Vec<&SymbolEntry> = symbols.iter().filter(|s| s.name == name).collect();

    match matches.len() {
        0 => {
            let top_names: Vec<&str> = symbols
                .iter()
                .filter(|s| s.indent == 0)
                .map(|s| s.name.as_str())
                .collect();
            AgentResult::Error {
                message: format!(
                    "symbol '{name}' not found in {path}. Top-level symbols: {}",
                    if top_names.is_empty() {
                        "(none)".to_string()
                    } else {
                        top_names.join(", ")
                    }
                ),
            }
        }
        1 => {
            let sym = matches[0];
            let start = sym.start_line as usize;
            let end = sym.end_line as usize;
            let lines: Vec<&str> = content.lines().collect();
            let slice = if start <= lines.len() && end <= lines.len() + 1 {
                &lines[start - 1..end.min(lines.len())]
            } else {
                &lines[start - 1..]
            };
            let mut source_code = slice.join("\n");
            let marker = format!("[lines {start}-{end} of {total_lines}]");
            source_code.push_str(&format!("\n{marker}"));
            if source_code.len() > MAX_OUTPUT {
                source_code = truncate_output_str(&source_code);
            }
            AgentResult::FileContent {
                content: source_code,
            }
        }
        _ => {
            // 多个同名符号：返回候选列表
            let candidates: Vec<String> = matches
                .iter()
                .map(|s| {
                    format!(
                        "[{}] {} \u{2014} lines {}-{}",
                        kind_label(&s.kind),
                        s.name,
                        s.start_line,
                        s.end_line
                    )
                })
                .collect();
            AgentResult::Error {
                message: format!(
                    "symbol '{name}' has {} matches in {path}:\n{}",
                    matches.len(),
                    candidates.join("\n")
                ),
            }
        }
    }
}

/// 字节级截断（与 client agent.rs 的 truncate_output 类似）
fn truncate_output_str(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let half = MAX_OUTPUT / 2;
    let mut head_end = half;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - half;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}\n[truncated]\n{}", &s[..head_end], &s[tail_start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outline_rust() {
        let source = r#"
fn main() {
    println!("hello");
}

struct Config {
    name: String,
}

impl Config {
    fn new() -> Self {
        Config { name: "test".into() }
    }
}

enum Color {
    Red,
    Green,
}

trait Drawable {
    fn draw(&self);
}
"#;
        let result = exec_outline(source, "src/main.rs");
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("[fn] main"));
                assert!(content.contains("[struct] Config"));
                assert!(content.contains("[impl] Config"));
                assert!(content.contains("[enum] Color"));
                assert!(content.contains("[trait] Drawable"));
                // 方法缩进
                assert!(content.contains("  [fn] new"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[test]
    fn test_outline_unsupported_extension() {
        let result = exec_outline("hello", "file.xyz");
        assert!(matches!(result, AgentResult::Error { .. }));
    }

    #[test]
    fn test_read_symbol_hit() {
        let source = "fn main() {\n    println!(\"hi\");\n}\n\nfn helper() {\n    let x = 1;\n}\n";
        let result = exec_read_symbol(source, "main.rs", "helper");
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("fn helper()"));
                assert!(content.contains("[lines 5-7 of"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[test]
    fn test_read_symbol_not_found() {
        let source = "fn main() {}\n";
        let result = exec_read_symbol(source, "main.rs", "nonexistent");
        assert!(matches!(result, AgentResult::Error { .. }));
    }

    #[test]
    fn test_outline_python() {
        let source = "def main():\n    pass\n\nclass Foo:\n    def bar(self):\n        pass\n";
        let result = exec_outline(source, "app.py");
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("main") || content.contains("Foo"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[test]
    fn test_outline_typescript() {
        let source = "function greet(name: string) {\n    console.log(name);\n}\n\nclass User {\n    constructor(public name: string) {}\n}\n";
        let result = exec_outline(source, "app.ts");
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("greet") || content.contains("User"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[test]
    fn test_outline_javascript() {
        let source = "function greet(name) {\n    console.log(name);\n}\n";
        let result = exec_outline(source, "app.js");
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("greet"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[test]
    fn test_outline_go() {
        let source = "package main\n\nfunc main() {\n}\n\ntype Config struct {\n    Name string\n}\n";
        let result = exec_outline(source, "main.go");
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("main") || content.contains("Config"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[test]
    fn test_read_symbol_multiple_candidates() {
        let source = "fn process() { }\nfn process_data() { }\n";
        // 'process' matches exactly once, 'process_data' once
        let r1 = exec_read_symbol(source, "main.rs", "process");
        assert!(matches!(r1, AgentResult::FileContent { .. }));
        // 'process_' prefix won't match anything
        let r2 = exec_read_symbol(source, "main.rs", "process_x");
        assert!(matches!(r2, AgentResult::Error { .. }));
    }
}
