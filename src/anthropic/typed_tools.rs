//! Anthropic 定义的工具（Anthropic-defined tools）适配
//!
//! `bash_20250124`、`computer_20250124`、`text_editor_20250728` 这类工具在请求里
//! 只写 `type`，不带 `input_schema`——schema 由官方 API 在服务端固定。Kiro 没有
//! 这套约定，原样转发过去就是一个空 schema 的函数，模型根本不知道能填什么，这些
//! 工具等于完全失效。
//!
//! 这里按官方文档给出它们各自的入参 schema 与说明。工具由客户端执行，所以名字和
//! 参数名保持与客户端一致，模型产出的 `tool_use` 可以原样交给客户端。

/// 一次工具适配的结果：发给上游的描述与 schema。
pub(crate) struct TypedToolSpec {
    pub description: &'static str,
    pub schema: serde_json::Value,
}

/// 按 `type` 前缀匹配 Anthropic 定义的工具。
///
/// 用前缀而不是全等，这样 `bash_20250124`、`text_editor_20250728` 之外的
/// 后续版本（如 `text_editor_20250429`）也能自动落到同一套 schema 上。
pub(crate) fn typed_tool_spec(tool_type: &str) -> Option<TypedToolSpec> {
    if tool_type.starts_with("bash_") {
        return Some(TypedToolSpec {
            description: "Run a shell command on the user's machine and return its output. \
                The client executes it and replies with the result.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to run."
                    },
                    "restart": {
                        "type": "boolean",
                        "description": "Set to true to restart the shell session before running the command."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        });
    }

    if tool_type.starts_with("text_editor_") || tool_type.starts_with("str_replace_based_edit_tool") {
        return Some(TypedToolSpec {
            description: "View and modify files on the user's machine. \
                Operations: `view` (show a file or directory), `create` (write a new file), \
                `str_replace` (replace an exact string), `insert` (insert text at a line). \
                The client executes it and replies with the result.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "enum": ["view", "create", "str_replace", "insert"],
                        "description": "The operation to perform."
                    },
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the file or directory."
                    },
                    "file_text": {
                        "type": "string",
                        "description": "`create` only: the full contents of the new file."
                    },
                    "old_str": {
                        "type": "string",
                        "description": "`str_replace` only: the exact text to replace. Must occur exactly once in the file."
                    },
                    "new_str": {
                        "type": "string",
                        "description": "`str_replace` / `insert` only: the replacement text, or the text to insert."
                    },
                    "insert_line": {
                        "type": "number",
                        "description": "`insert` only: the line number after which to insert the text (0 inserts at the top)."
                    },
                    "view_range": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "`view` only: [start_line, end_line], 1-indexed; end_line -1 means end of file."
                    }
                },
                "required": ["command", "path"],
                "additionalProperties": false
            }),
        });
    }

    if tool_type.starts_with("computer_") {
        return Some(TypedToolSpec {
            description: "Control the user's screen with mouse and keyboard. \
                The client executes it and replies with a screenshot plus cursor coordinates.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "key", "type", "mouse_move", "left_click", "left_click_drag",
                            "right_click", "middle_click", "double_click", "triple_click",
                            "scroll", "left_mouse_down", "left_mouse_up", "hold_key", "wait",
                            "screenshot", "cursor_position"
                        ],
                        "description": "The action to perform."
                    },
                    "coordinate": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "[x, y] pixel coordinate, for coordinate-based actions."
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type, or the key / key combination to press."
                    },
                    "scroll_direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"],
                        "description": "`scroll` only: the direction to scroll."
                    },
                    "scroll_amount": {
                        "type": "number",
                        "description": "`scroll` only: how many clicks to scroll."
                    },
                    "duration": {
                        "type": "number",
                        "description": "`hold_key` / `wait` only: duration in seconds."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_schema_requires_command() {
        let spec = typed_tool_spec("bash_20250124").unwrap();
        assert_eq!(spec.schema["required"], serde_json::json!(["command"]));
        assert!(spec.schema["properties"]["command"].is_object());
    }

    #[test]
    fn text_editor_versions_share_the_schema() {
        let current = typed_tool_spec("text_editor_20250728").unwrap();
        let older = typed_tool_spec("text_editor_20250429").unwrap();
        assert_eq!(current.schema, older.schema);
        assert!(
            current.schema["properties"]["command"]["enum"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("str_replace"))
        );
    }

    #[test]
    fn computer_schema_lists_actions() {
        let spec = typed_tool_spec("computer_20250124").unwrap();
        assert_eq!(spec.schema["required"], serde_json::json!(["action"]));
    }

    #[test]
    fn web_search_and_plain_types_are_not_typed_tools() {
        // web_search 走自己的搜索通道，不在这里处理
        assert!(typed_tool_spec("web_search_20250305").is_none());
        assert!(typed_tool_spec("custom").is_none());
    }
}
