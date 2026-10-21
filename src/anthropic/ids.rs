//! Anthropic 风格的标识符生成
//!
//! 真实 Claude API 返回的 id 形如 `msg_01` / `toolu_01` / `srvtoolu_01` 前缀加
//! 22 位 base62 字符；thinking 块的 `signature` 是一段 base64 编码的不透明字节。
//! 这里统一生成这些格式，避免客户端看到 uuid 或占位符这类明显的代理痕迹。

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 前缀之后的随机部分长度（与官方 id 一致）
const ID_BODY_LEN: usize = 22;

fn random_base62(len: usize) -> String {
    (0..len)
        .map(|_| BASE62[fastrand::usize(..BASE62.len())] as char)
        .collect()
}

fn prefixed(prefix: &str) -> String {
    format!("{}{}", prefix, random_base62(ID_BODY_LEN))
}

/// `msg_01...` 消息 id
pub fn message_id() -> String {
    prefixed("msg_01")
}

/// `toolu_01...` 客户端工具调用 id
pub fn tool_use_id() -> String {
    prefixed("toolu_01")
}

/// `srvtoolu_01...` 服务端工具（web_search）调用 id
pub fn server_tool_use_id() -> String {
    prefixed("srvtoolu_01")
}

/// `req_011...` 请求 id（错误响应体中的 `request_id`）
pub fn request_id() -> String {
    format!("req_011{}", random_base62(ID_BODY_LEN - 1))
}

/// 把上游（Kiro）给出的 toolUseId 映射为 Anthropic 风格的 id。
///
/// 映射是确定性的：同一个上游 id 永远得到同一个 `toolu_01...`，因此客户端在
/// 下一轮把 tool_result 带回来时，转换历史得到的 id 仍然前后一致。已是
/// Anthropic 风格的 id 原样保留。
pub fn tool_use_id_from_upstream(upstream_id: &str) -> String {
    if upstream_id.starts_with("toolu_") || upstream_id.starts_with("srvtoolu_") {
        return upstream_id.to_string();
    }
    let digest = Sha256::digest(upstream_id.as_bytes());
    let body: String = digest
        .iter()
        .take(ID_BODY_LEN)
        .map(|b| BASE62[(*b as usize) % BASE62.len()] as char)
        .collect();
    format!("toolu_01{}", body)
}

/// 生成 thinking 块的 `signature`。
///
/// 官方签名是加密后的 protobuf 字节再 base64 编码，客户端只透传、不解析。
/// 这里按相同的外形（字段 2 的 length-delimited 字节串）生成随机内容。
pub fn thinking_signature() -> String {
    let payload_len = fastrand::usize(200..=400);
    let mut bytes = Vec::with_capacity(payload_len + 4);
    bytes.push(0x12);
    let mut len = payload_len;
    loop {
        let byte = (len & 0x7f) as u8;
        len >>= 7;
        if len == 0 {
            bytes.push(byte);
            break;
        }
        bytes.push(byte | 0x80);
    }
    bytes.extend((0..payload_len).map(|_| fastrand::u8(..)));
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_anthropic_id(id: &str, prefix: &str) {
        assert!(id.starts_with(prefix), "{id} should start with {prefix}");
        let body = &id[prefix.len()..];
        assert_eq!(body.len(), ID_BODY_LEN, "{id}");
        assert!(body.bytes().all(|b| b.is_ascii_alphanumeric()), "{id}");
    }

    #[test]
    fn generated_ids_use_anthropic_shape() {
        assert_anthropic_id(&message_id(), "msg_01");
        assert_anthropic_id(&tool_use_id(), "toolu_01");
        assert_anthropic_id(&server_tool_use_id(), "srvtoolu_01");
        let req = request_id();
        assert!(req.starts_with("req_011") && req.len() == 28, "{req}");
        assert_ne!(message_id(), message_id());
    }

    #[test]
    fn upstream_tool_ids_map_deterministically() {
        let a = tool_use_id_from_upstream("tooluse_abc123");
        assert_anthropic_id(&a, "toolu_01");
        assert_eq!(a, tool_use_id_from_upstream("tooluse_abc123"));
        assert_ne!(a, tool_use_id_from_upstream("tooluse_abc124"));
        assert_eq!(
            tool_use_id_from_upstream("toolu_01AbCdEf"),
            "toolu_01AbCdEf"
        );
        assert_eq!(
            tool_use_id_from_upstream("srvtoolu_01xyz"),
            "srvtoolu_01xyz"
        );
    }

    #[test]
    fn thinking_signature_is_base64_with_protobuf_header() {
        let sig = thinking_signature();
        let bytes = STANDARD.decode(&sig).expect("valid base64");
        assert_eq!(bytes[0], 0x12);
        assert!(bytes.len() > 200);
        assert_ne!(sig, thinking_signature());
    }
}
