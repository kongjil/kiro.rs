//! PDF 文字层提取
//!
//! Kiro 上游只接受文本和图片，没有文档通道。客户端发来的 `document` 块如果是
//! PDF，直接转发只能得到一句「加载失败」，模型看不到文件内容，却常常顺着
//! 上下文编出一份并不存在的摘要——这是最坑人的一类降智。
//!
//! 这里在代理内部把 PDF 的文字层抽出来当普通文本转发：
//! - 解析流对象，支持 `FlateDecode`（绝大多数 PDF）、`ASCIIHexDecode`、
//!   `ASCII85Decode` 与未压缩流；
//! - 按内容流里的 `Tj` / `TJ` / `'` / `"` 取字符串，`Td` / `TD` / `T*` / `ET`
//!   断行；
//! - 有 `ToUnicode` CMap 时按 CMap 解码（中文等 CID 字体必须靠它）；
//! - 提取结果做可读性检查，扫描件（纯图片、无文字层）或乱码一律判定为失败，
//!   由调用方给出「内容没有读到」的明确提示，而不是把垃圾塞给模型。

use std::collections::HashMap;

/// 单个 PDF 最多抽取的字符数，超出截断（防止一份超长 PDF 撑爆上游上下文）。
const MAX_TEXT_CHARS: usize = 200_000;

/// 抽取 PDF 文字层。返回 `None` 表示没有可用的文字层（扫描件、或编码无法还原）。
pub(crate) fn extract_pdf_text(data: &[u8]) -> Option<String> {
    let cmap = parse_cmap(data);
    let mut chunks = Vec::new();

    for stream in decoded_streams(data) {
        if stream.is_empty() || !looks_like_content_stream(&stream) {
            continue;
        }
        let text = collect_text(&stream, cmap.as_ref());
        if !text.trim().is_empty() {
            chunks.push(text);
        }
    }

    let joined = normalize_text(&chunks.join("\n\n"));
    if !is_readable(&joined) {
        return None;
    }
    Some(truncate_chars(&joined, MAX_TEXT_CHARS))
}

/// 把可能解压失败的流统一成可读字节序列。
fn decoded_streams(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(pos) = find(&data[cursor..], b"stream") {
        let stream_kw = cursor + pos;
        // `endstream` 里也含 `stream`，跳过它。
        if stream_kw >= 3 && &data[stream_kw - 3..stream_kw] == b"end" {
            cursor = stream_kw + 6;
            continue;
        }

        // 数据起点：`stream` 后跟 \r\n、\n 或单独 \r。
        let mut start = stream_kw + 6;
        if data.get(start) == Some(&b'\r') {
            start += 1;
        }
        if data.get(start) == Some(&b'\n') {
            start += 1;
        }
        let Some(len_pos) = find(&data[start..], b"endstream") else {
            break;
        };
        let end = start + len_pos;
        let dict_start = data[..stream_kw]
            .windows(2)
            .rposition(|window| window == b"obj")
            .map(|i| i + 3)
            .unwrap_or(0);
        let dict = &data[dict_start..stream_kw.min(data.len())];

        let raw = &data[start..end];
        if let Some(decoded) = decode_stream(raw, dict) {
            out.push(decoded);
        }
        cursor = end + 9;
    }
    out
}

fn decode_stream(raw: &[u8], dict: &[u8]) -> Option<Vec<u8>> {
    if contains(dict, b"FlateDecode") {
        let mut out = Vec::new();
        let mut decoder = flate2::read::ZlibDecoder::new(raw);
        if std::io::Read::read_to_end(&mut decoder, &mut out).is_ok() && !out.is_empty() {
            return Some(out);
        }
        // 少数生成器写的是无 zlib 头的裸 deflate。
        let mut out = Vec::new();
        let mut decoder = flate2::read::DeflateDecoder::new(raw);
        if std::io::Read::read_to_end(&mut decoder, &mut out).is_ok() && !out.is_empty() {
            return Some(out);
        }
        return None;
    }
    if contains(dict, b"ASCIIHexDecode") {
        return Some(decode_ascii_hex(raw));
    }
    if contains(dict, b"ASCII85Decode") {
        return Some(decode_ascii85(raw));
    }
    // 其他过滤器（图片的 DCTDecode、字体子集等）交给可读性检查过滤。
    Some(raw.to_vec())
}

fn decode_ascii_hex(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut high: Option<u8> = None;
    for byte in raw {
        let value = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            b'>' => break,
            _ => continue,
        };
        match high.take() {
            Some(h) => out.push(h << 4 | value),
            None => high = Some(value),
        }
    }
    out
}

fn decode_ascii85(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group = Vec::with_capacity(5);
    for &byte in raw {
        match byte {
            b'z' if group.is_empty() => out.extend_from_slice(&[0, 0, 0, 0]),
            b'~' => break,
            b'!'..=b'u' => {
                group.push(byte - b'!');
                if group.len() == 5 {
                    push_ascii85_group(&mut out, &group[..], 4);
                    group.clear();
                }
            }
            _ => {}
        }
    }
    if !group.is_empty() {
        let written = group.len().saturating_sub(1);
        group.resize(5, 84);
        push_ascii85_group(&mut out, &group[..], written);
    }
    out
}

fn push_ascii85_group(out: &mut Vec<u8>, group: &[u8], written: usize) {
    let mut value = 0u32;
    for &digit in group {
        value = value.wrapping_mul(85).wrapping_add(u32::from(digit));
    }
    let bytes = value.to_be_bytes();
    out.extend_from_slice(&bytes[..written.min(4)]);
}

fn looks_like_content_stream(stream: &[u8]) -> bool {
    contains(stream, b"BT") && (contains(stream, b"Tj") || contains(stream, b"TJ"))
}

// === ToUnicode CMap ===

struct CMap {
    /// 源字节码 -> Unicode 字符串
    map: HashMap<u32, String>,
    /// 源字节码是否为双字节（CID 字体）
    two_byte: bool,
}

fn parse_cmap(data: &[u8]) -> Option<CMap> {
    for stream in decoded_streams(data) {
        if !contains(&stream, b"beginbfchar") && !contains(&stream, b"beginbfrange") {
            continue;
        }
        let text = String::from_utf8_lossy(&stream).into_owned();
        let mut map = HashMap::new();
        let mut two_byte = false;

        let mut rest = text.as_str();
        while let Some(start) = rest.find("beginbfchar") {
            rest = &rest[start + "beginbfchar".len()..];
            let Some(end) = rest.find("endbfchar") else {
                break;
            };
            for (src, dst) in parse_bfchar_pairs(&rest[..end]) {
                two_byte |= src > 0xFF;
                map.insert(src, dst);
            }
            rest = &rest[end + "endbfchar".len()..];
        }

        let mut rest = text.as_str();
        while let Some(start) = rest.find("beginbfrange") {
            rest = &rest[start + "beginbfrange".len()..];
            let Some(end) = rest.find("endbfrange") else {
                break;
            };
            for (src, dst) in parse_bfrange_triples(&rest[..end]) {
                two_byte |= src > 0xFF;
                map.insert(src, dst);
            }
            rest = &rest[end + "endbfrange".len()..];
        }

        if !map.is_empty() {
            return Some(CMap { map, two_byte });
        }
    }
    None
}

fn parse_bfchar_pairs(body: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut tokens = body.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        let Some(code) = hex_code(token) else {
            continue;
        };
        let Some(dest) = tokens.next() else {
            break;
        };
        if let Some(name) = hex_string_text(dest) {
            out.push((code, name));
        }
    }
    out
}

fn parse_bfrange_triples(body: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut tokens = body.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        let Some(low) = hex_code(token) else {
            continue;
        };
        let Some(high) = tokens.next().and_then(hex_code) else {
            break;
        };
        // 第三种写法可能是 `<dst>`，也可能是 `[<a> <b> ...]`
        let Some(array_start) = tokens.peek().copied() else {
            break;
        };
        if array_start.starts_with('[') {
            // 展开数组，逐个对应 low..=high
            let mut code = low;
            while let Some(item) = tokens.next() {
                let item = item.trim_start_matches('[');
                if item.is_empty() {
                    continue;
                }
                if let Some(text) = hex_string_text(item) {
                    out.push((code, text));
                    code = code.saturating_add(1);
                }
                if item.ends_with(']') {
                    break;
                }
            }
            continue;
        }

        tokens.next();
        let Some(mut base) = hex_string_bytes(array_start) else {
            continue;
        };
        if high > low.saturating_add(65_535) {
            continue;
        }
        for code in low..=high {
            let text = String::from_utf16_lossy(
                &base
                    .chunks(2)
                    .map(|pair| {
                        if pair.len() == 2 {
                            u16::from_be_bytes([pair[0], pair[1]])
                        } else {
                            u16::from(pair[0])
                        }
                    })
                    .collect::<Vec<_>>(),
            );
            out.push((code, text));
            // 递增目的码的末字节
            if let Some(last) = base.last_mut() {
                *last = last.wrapping_add(1);
            }
        }
    }
    out
}

fn hex_code(token: &str) -> Option<u32> {
    let inner = token.strip_prefix('<')?.strip_suffix('>')?;
    if inner.is_empty() {
        return None;
    }
    let bytes = hex_bytes(inner)?;
    Some(bytes.iter().fold(0u32, |acc, b| acc << 8 | u32::from(*b)))
}

fn hex_string_text(token: &str) -> Option<String> {
    let bytes = hex_string_bytes(token)?;
    Some(utf16_or_latin1(&bytes))
}

fn hex_string_bytes(token: &str) -> Option<Vec<u8>> {
    let inner = token.strip_prefix('<')?.strip_suffix('>')?;
    hex_bytes(inner)
}

fn hex_bytes(inner: &str) -> Option<Vec<u8>> {
    let mut digits = inner
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if !digits.len().is_multiple_of(2) {
        digits.push(b'0');
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    let mut index = 0;
    while index + 1 < digits.len() {
        let high = (digits[index] as char).to_digit(16)?;
        let low = (digits[index + 1] as char).to_digit(16)?;
        out.push((high as u8) << 4 | low as u8);
        index += 2;
    }
    Some(out)
}

fn utf16_or_latin1(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len().is_multiple_of(2) {
        let units = bytes
            .chunks(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        // 纯 Latin-1 范围的高字节恒为 0；出现明确非零高位时按 UTF-16BE
        if units.iter().any(|u| *u > 0xFF) {
            return String::from_utf16_lossy(&units);
        }
    }
    bytes.iter().map(|b| *b as char).collect()
}

// === 内容流取字 ===

fn collect_text(stream: &[u8], cmap: Option<&CMap>) -> String {
    let mut out = String::new();
    let mut index = 0usize;
    let mut pending: Option<Vec<u8>> = None;
    // `[ ... ] TJ` 里每段字符串本身就是一次显示
    let mut in_array = false;

    while index < stream.len() {
        match stream[index] {
            b'(' => {
                let (bytes, next) = read_literal_string(stream, index + 1);
                emit(&mut out, &bytes, cmap, &mut pending, in_array);
                index = next;
            }
            b'<' if stream.get(index + 1) != Some(&b'<') => {
                let (bytes, next) = read_hex_string(stream, index + 1);
                emit(&mut out, &bytes, cmap, &mut pending, in_array);
                index = next;
            }
            b'%' => {
                while index < stream.len() && stream[index] != b'\n' {
                    index += 1;
                }
            }
            b'[' => {
                in_array = true;
                index += 1;
            }
            b']' => {
                in_array = false;
                index += 1;
            }
            _ => {
                if is_operator_char(stream[index]) {
                    let start = index;
                    while index < stream.len() && is_operator_char(stream[index]) {
                        index += 1;
                    }
                    apply_operator(&stream[start..index], &mut out, &mut pending, cmap);
                    continue;
                }
                index += 1;
            }
        }
    }

    // 结尾残留的字符串按显示处理
    if let Some(bytes) = pending.take() {
        out.push_str(&decode_string(&bytes, cmap));
    }
    out
}

fn is_operator_char(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'*' | b'\'' | b'"')
}

fn apply_operator(
    op: &[u8],
    out: &mut String,
    pending: &mut Option<Vec<u8>>,
    cmap: Option<&CMap>,
) {
    match op {
        b"Tj" | b"'" | b"\"" | b"TJ" => {
            if let Some(bytes) = pending.take() {
                out.push_str(&decode_string(&bytes, cmap));
            }
            if matches!(op, b"'" | b"\"") {
                out.push('\n');
            }
        }
        b"Td" | b"TD" | b"T*" | b"ET" => {
            out.push('\n');
        }
        _ => {}
    }
}

/// 把刚读到的字符串落到输出：数组里立即显示，否则先挂起等 `Tj`。
fn emit(
    out: &mut String,
    bytes: &[u8],
    cmap: Option<&CMap>,
    pending: &mut Option<Vec<u8>>,
    in_array: bool,
) {
    if in_array {
        out.push_str(&decode_string(bytes, cmap));
        return;
    }
    if let Some(previous) = pending.take() {
        out.push_str(&decode_string(&previous, cmap));
    }
    *pending = Some(bytes.to_vec());
}

fn read_literal_string(stream: &[u8], mut index: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut depth = 1usize;
    while index < stream.len() {
        let byte = stream[index];
        index += 1;
        match byte {
            b'\\' => {
                let Some(&escaped) = stream.get(index) else {
                    break;
                };
                index += 1;
                match escaped {
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'\n' => {}
                    b'\r' => {
                        if stream.get(index) == Some(&b'\n') {
                            index += 1;
                        }
                    }
                    b'0'..=b'7' => {
                        let mut value = u16::from(escaped - b'0');
                        let mut taken = 0;
                        while taken < 2 {
                            match stream.get(index) {
                                Some(digit @ b'0'..=b'7') => {
                                    value = value * 8 + u16::from(digit - b'0');
                                    index += 1;
                                    taken += 1;
                                }
                                _ => break,
                            }
                        }
                        out.push(value as u8);
                    }
                    other => out.push(other),
                }
            }
            b'(' => {
                depth += 1;
                out.push(byte);
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                out.push(byte);
            }
            _ => out.push(byte),
        }
    }
    (out, index)
}

fn read_hex_string(stream: &[u8], mut index: usize) -> (Vec<u8>, usize) {
    let mut digits = Vec::new();
    while index < stream.len() && stream[index] != b'>' {
        if !stream[index].is_ascii_whitespace() {
            digits.push(stream[index]);
        }
        index += 1;
    }
    if index < stream.len() {
        index += 1;
    }
    (hex_bytes(&String::from_utf8_lossy(&digits)).unwrap_or_default(), index)
}

fn decode_string(bytes: &[u8], cmap: Option<&CMap>) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units = bytes[2..]
            .chunks(2)
            .map(|pair| {
                if pair.len() == 2 {
                    u16::from_be_bytes([pair[0], pair[1]])
                } else {
                    u16::from(pair[0])
                }
            })
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&units);
    }

    let Some(cmap) = cmap else {
        return bytes.iter().map(|b| *b as char).collect();
    };

    let mut out = String::new();
    if cmap.two_byte {
        for pair in bytes.chunks(2) {
            let code = if pair.len() == 2 {
                u32::from(u16::from_be_bytes([pair[0], pair[1]]))
            } else {
                u32::from(pair[0])
            };
            match cmap.map.get(&code) {
                Some(text) => out.push_str(text),
                None => out.push_str(&String::from_utf16_lossy(&[code as u16])),
            }
        }
    } else {
        for byte in bytes {
            match cmap.map.get(&u32::from(*byte)) {
                Some(text) => out.push_str(text),
                None => out.push(*byte as char),
            }
        }
    }
    out
}

// === 结果整理 ===

fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_lines = 0;
    for line in text.lines() {
        let trimmed = line.trim_end();
        let cleaned: String = trimmed
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        if cleaned.trim().is_empty() {
            blank_lines += 1;
            if blank_lines > 1 {
                continue;
            }
        } else {
            blank_lines = 0;
        }
        out.push_str(cleaned.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

/// 判定抽取结果是否真的可读：太短、或奇怪字符占比过高都认为没抽出来。
fn is_readable(text: &str) -> bool {
    let total = text.chars().count();
    if total < 4 {
        return false;
    }
    let meaningful = text
        .chars()
        .filter(|c| {
            c.is_ascii_graphic()
                || c.is_ascii_whitespace()
                || (!c.is_control() && !c.is_ascii() && *c != '\u{FFFD}')
        })
        .count();
    let replacement = text.chars().filter(|c| *c == '\u{FFFD}').count();
    meaningful * 100 >= total * 90 && replacement * 100 < total
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("\n\n[document truncated: the PDF is longer than the readable limit]");
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用未压缩的内容流拼一份最小 PDF。
    fn pdf_with(content: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n");
        out.extend_from_slice(
            format!(
                "2 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                content.len(),
                content
            )
            .as_bytes(),
        );
        out.extend_from_slice(b"%%EOF\n");
        out
    }

    #[test]
    fn extracts_plain_text_operators() {
        let pdf = pdf_with("BT /F1 12 Tf 72 720 Td (Hello PDF) Tj ET");
        assert_eq!(extract_pdf_text(&pdf).as_deref(), Some("Hello PDF"));
    }

    #[test]
    fn joins_arrays_and_breaks_lines_between_text_blocks() {
        let content = "BT (First line, ) Tj [(with) -250 (an array)] TJ ET \
                       BT 0 -14 Td (Second paragraph that is long enough to pass the readability check.) Tj ET";
        let pdf = pdf_with(content);
        let text = extract_pdf_text(&pdf).unwrap();
        assert!(text.contains("First line, withan array"), "{text}");
        assert!(text.contains("Second paragraph"), "{text}");
        assert!(text.lines().count() >= 2, "{text}");
    }

    #[test]
    fn decodes_flate_compressed_streams() {
        use flate2::{Compression, write::ZlibEncoder};
        use std::io::Write;

        let content = "BT (Compressed text is extracted just fine.) Tj ET";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        pdf.extend_from_slice(b"2 0 obj\n<< /Filter /FlateDecode >>\nstream\n");
        pdf.extend_from_slice(&compressed);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        assert_eq!(
            extract_pdf_text(&pdf).as_deref(),
            Some("Compressed text is extracted just fine.")
        );
    }

    #[test]
    fn decodes_cid_text_through_to_unicode_cmap() {
        // 1 字节码 -> Unicode 的 CMap，模拟中文嵌入字体的 ToUnicode 表
        let cmap = "/CIDInit /ProcSet findresource begin\n\
                    1 begincodespacerange <00> <FF> endcodespacerange\n\
                    4 beginbfchar\n\
                    <01> <4F60>\n\
                    <02> <597D>\n\
                    <03> <4E16>\n\
                    <04> <754C>\n\
                    endbfchar\n\
                    end";
        let content = "BT <01020304> Tj ET";
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        pdf.extend_from_slice(
            format!(
                "3 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                cmap.len(),
                cmap
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                content.len(),
                content
            )
            .as_bytes(),
        );
        let text = extract_pdf_text(&pdf).unwrap();
        assert!(text.contains("你好世界"), "{text}");
    }

    #[test]
    fn hex_strings_and_literal_escapes_survive() {
        let content = "BT (A \\(quoted\\) line with enough text to pass.) Tj <48656C6C6F> Tj ET";
        let pdf = pdf_with(content);
        let text = extract_pdf_text(&pdf).unwrap();
        assert!(text.contains("A (quoted) line"), "{text}");
        assert!(text.contains("Hello"), "{text}");
    }

    #[test]
    fn image_only_pdf_has_no_text_layer() {
        // 扫描件：只有图片流，没有任何 BT/Tj
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n5 0 obj\n<< /Filter /DCTDecode >>\nstream\n");
        pdf.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]);
        pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");
        assert_eq!(extract_pdf_text(&pdf), None);
    }

    #[test]
    fn ascii_hex_filter_is_decoded() {
        let content = "BT (Hex filter content that is long enough.) Tj ET";
        let encoded: String = content.bytes().map(|b| format!("{b:02X}")).collect();
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n6 0 obj\n<< /Filter /ASCIIHexDecode >>\nstream\n");
        pdf.extend_from_slice(encoded.as_bytes());
        pdf.extend_from_slice(b">\nendstream\nendobj\n");
        assert!(
            extract_pdf_text(&pdf)
                .unwrap()
                .contains("Hex filter content")
        );
    }

    #[test]
    fn truncation_marks_the_cut() {
        let long = "x".repeat(MAX_TEXT_CHARS + 100);
        let pdf = pdf_with(&format!("BT ({long}) Tj ET"));
        let text = extract_pdf_text(&pdf).unwrap();
        assert!(text.contains("document truncated"), "{}", &text[..40]);
    }
}
