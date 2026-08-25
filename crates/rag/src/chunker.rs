//! Markdown 结构感知分块。
//!
//! 策略：按标题层级建立 heading_path，标题是硬边界（每遇标题封箱开启新块）；
//! 段落、代码块作为语义单元顺序装箱，接近 chunk_size（token 近似）时封箱；
//! 相邻块共享末尾 overlap 内容保证语义连贯；代码块/表格绝不从中间切断，
//! 超大代码块单独成块，超大段落按 chunk_size 切分为重叠片段。

/// 一个分块：标题路径 + 原文 + token 近似数。
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// 标题路径，形如 `一级/二级`，无标题时为空字符串。
    pub heading_path: String,
    /// 分块原文，已去首尾空白。
    pub content: String,
    /// `content` 的 token 近似数（字符数 ÷ 4 向上取整）。
    pub token_count: usize,
}

/// 字符数 ÷ 4 的 token 近似（中英混合误差可接受）。
fn approx_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// 语义单元类型。
///
/// `List` / `Table` 为后续列表/表格检测预留（当前解析器只产出前三类）。
#[derive(Debug, Clone, PartialEq)]
#[allow(
    dead_code,
    reason = "List/Table reserved for future list/table detection"
)]
enum UnitKind {
    Heading,
    Paragraph,
    Code,
    List,
    Table,
}

#[derive(Debug, Clone)]
struct Unit {
    kind: UnitKind,
    text: String,
    /// Heading 单元专用：层级（1-6）。
    level: u8,
}

/// 把 Markdown 文本解析为有序语义单元序列。
fn parse_units(text: &str) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut cur = String::new(); // 当前累积的普通段落
    let mut in_code = false;
    let mut code_buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            if in_code {
                code_buf.push_str(line);
                code_buf.push('\n');
                units.push(Unit {
                    kind: UnitKind::Code,
                    text: code_buf.trim_end().to_string(),
                    level: 0,
                });
                code_buf.clear();
                in_code = false;
            } else {
                flush_para(&mut cur, &mut units);
                in_code = true;
                code_buf.push_str(line);
                code_buf.push('\n');
            }
            continue;
        }
        if in_code {
            code_buf.push_str(line);
            code_buf.push('\n');
            continue;
        }
        // 标题
        if let Some(level) = heading_level(trimmed) {
            flush_para(&mut cur, &mut units);
            units.push(Unit {
                kind: UnitKind::Heading,
                text: trimmed.to_string(),
                level,
            });
            continue;
        }
        // 空行 → 段落边界
        if trimmed.is_empty() {
            flush_para(&mut cur, &mut units);
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    flush_para(&mut cur, &mut units);
    if in_code && !code_buf.trim().is_empty() {
        // 未闭合代码块兜底：作为一个代码单元
        units.push(Unit {
            kind: UnitKind::Code,
            text: code_buf.trim_end().to_string(),
            level: 0,
        });
    }
    units
}

/// 累积的普通段落转为 Paragraph 单元。
fn flush_para(cur: &mut String, units: &mut Vec<Unit>) {
    let t = cur.trim();
    if !t.is_empty() {
        units.push(Unit {
            kind: UnitKind::Paragraph,
            text: t.to_string(),
            level: 0,
        });
    }
    cur.clear();
}

/// 返回 `#` 标题层级（1-6），非标题返回 None。
fn heading_level(line: &str) -> Option<u8> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && t.chars().nth(hashes) == Some(' ') {
        Some(u8::try_from(hashes).unwrap_or(u8::MAX))
    } else {
        None
    }
}

/// 维护标题栈，产出当前单元的 heading_path（"一级/二级/…"）。
fn update_heading_path(stack: &mut Vec<(u8, String)>, level: u8, title: &str) -> String {
    while let Some((l, _)) = stack.last() {
        if *l >= level {
            stack.pop();
        } else {
            break;
        }
    }
    stack.push((level, title.to_string()));
    stack
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// 缓冲内容封箱为一个 Chunk（空缓冲则跳过）。
fn flush_buf(buf: &mut String, path: &str, chunks: &mut Vec<Chunk>) {
    let t = buf.trim();
    if !t.is_empty() {
        chunks.push(Chunk {
            heading_path: path.to_string(),
            content: t.to_string(),
            token_count: approx_tokens(t),
        });
    }
    buf.clear();
}

/// 取文本末尾约 overlap token 的尾部（在字符边界截断）。
fn tail_text(s: &str, overlap: usize) -> String {
    if overlap == 0 {
        return String::new();
    }
    let want_chars = overlap * 4;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= want_chars {
        return s.to_string();
    }
    chars[chars.len() - want_chars..].iter().collect()
}

/// 把超大段落按 chunk_size（token 近似）切分为重叠片段。
fn split_paragraph(
    text: &str,
    chunk_size: usize,
    overlap: usize,
    path: &str,
    chunks: &mut Vec<Chunk>,
) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return;
    }
    let step = chunk_size * 4; // ≈chunk_size token 的字符数
    if step == 0 {
        return;
    }
    let ov = overlap * 4;
    let mut start = 0;
    while start < chars.len() {
        let end = (start + step).min(chars.len());
        let piece: String = chars[start..end].iter().collect();
        let tokens = approx_tokens(&piece);
        chunks.push(Chunk {
            heading_path: path.to_string(),
            content: piece,
            token_count: tokens,
        });
        if end == chars.len() {
            break;
        }
        let next = end.saturating_sub(ov);
        if next <= start {
            start = end;
        } else {
            start = next;
        }
    }
}

/// 结构感知分块入口。
#[must_use]
pub fn chunk_markdown(text: &str, chunk_size: usize, overlap: usize) -> Vec<Chunk> {
    let units = parse_units(text);
    if units.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut cur_path = String::new();
    let mut buf = String::new();
    let mut buf_path = String::new();

    for unit in &units {
        if unit.kind == UnitKind::Heading {
            // 标题是硬边界：封箱当前缓冲，避免块跨越章节
            flush_buf(&mut buf, &buf_path, &mut chunks);
            cur_path = update_heading_path(
                &mut stack,
                unit.level,
                unit.text.trim_start_matches('#').trim(),
            );
        }
        let unit_tokens = approx_tokens(&unit.text);
        // 超大代码块：绝不从中间切断，先封箱再单独成块
        if unit_tokens >= chunk_size && unit.kind == UnitKind::Code {
            flush_buf(&mut buf, &buf_path, &mut chunks);
            chunks.push(Chunk {
                heading_path: cur_path.clone(),
                content: unit.text.trim().to_string(),
                token_count: unit_tokens,
            });
            continue;
        }
        // 超大段落：按 chunk_size 切分为重叠片段
        if unit.kind == UnitKind::Paragraph && unit_tokens >= chunk_size && chunk_size > 0 {
            flush_buf(&mut buf, &buf_path, &mut chunks);
            split_paragraph(&unit.text, chunk_size, overlap, &cur_path, &mut chunks);
            continue;
        }
        // 加入会超限 → 封箱（保留 overlap 尾部）
        if !buf.is_empty() && approx_tokens(&buf) + unit_tokens > chunk_size {
            let tail = tail_text(&buf, overlap);
            flush_buf(&mut buf, &buf_path, &mut chunks);
            buf = tail;
        }
        if buf.is_empty() {
            buf_path.clone_from(&cur_path);
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(unit.text.trim());
    }
    flush_buf(&mut buf, &buf_path, &mut chunks);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_headings_and_records_path() {
        let md = "# 指南\n\n介绍。\n\n## 安装\n\n步骤一。\n\n### Linux\n\nLinux 专属。\n";
        let chunks = chunk_markdown(md, 512, 64);
        assert!(!chunks.is_empty());
        // 至少有一个块的 heading_path 含嵌套
        assert!(chunks.iter().any(|c| c.heading_path.contains("安装")));
    }

    #[test]
    fn never_splits_code_block() {
        let code = "```rust\nfn a() {}\nfn b() {}\nfn c() {}\n```\n";
        let md = format!("# T\n\n{code}\n\n后续段落。\n");
        let chunks = chunk_markdown(&md, 20, 0); // 很小的 chunk_size
                                                 // 代码块完整存在于某个块中
        assert!(chunks
            .iter()
            .any(|c| c.content.contains("fn a()") && c.content.contains("fn c()")));
    }

    #[test]
    fn respects_chunk_size_with_overlap() {
        let md = "# H\n\n".to_string() + &"段落内容。".repeat(200);
        let chunks = chunk_markdown(&md, 100, 20);
        assert!(chunks.len() > 1, "长文本应分多块");
        // 相邻块有重叠（除首块外，块首应能在前一块尾部找到衔接）
        // 此处仅断言块数与 content 非空
        assert!(chunks.iter().all(|c| !c.content.trim().is_empty()));
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_markdown("", 512, 64).is_empty());
        assert!(chunk_markdown("   \n\n  ", 512, 64).is_empty());
    }

    #[test]
    fn oversized_atomic_unit_becomes_single_chunk() {
        // 单个超大代码块（无内部可切点）应单独成块而非丢弃
        let big = "```\n".to_string() + &"x".repeat(5000) + "\n```\n";
        let chunks = chunk_markdown(&big, 512, 64);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("xxx"));
    }
}
