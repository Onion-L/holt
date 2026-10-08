//! Port of `pi-core/ai/src/utils/text.ts`: text extraction from message
//! content, plus `shortHash` from `utils/hash.ts`.

/// Content shapes accepted by [`content_text`].
pub trait TextContentBlocks {
    /// The text of `text` blocks, in order.
    fn text_blocks(&self) -> Vec<String>;
}

impl TextContentBlocks for &str {
    fn text_blocks(&self) -> Vec<String> {
        vec![(*self).to_string()]
    }
}

impl TextContentBlocks for String {
    fn text_blocks(&self) -> Vec<String> {
        vec![self.clone()]
    }
}

impl TextContentBlocks for &crate::ai::types::UserContent {
    fn text_blocks(&self) -> Vec<String> {
        match self {
            crate::ai::types::UserContent::Text(text) => vec![text.clone()],
            crate::ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    crate::ai::types::BlockContent::Text(text) => Some(text.text.clone()),
                    crate::ai::types::BlockContent::Image(_) => None,
                })
                .collect(),
        }
    }
}

impl TextContentBlocks for &[crate::ai::types::AssistantContent] {
    fn text_blocks(&self) -> Vec<String> {
        self.iter()
            .filter_map(|block| match block {
                crate::ai::types::AssistantContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl TextContentBlocks for &Vec<crate::ai::types::AssistantContent> {
    fn text_blocks(&self) -> Vec<String> {
        self.as_slice().text_blocks()
    }
}

impl TextContentBlocks for &[crate::ai::types::BlockContent] {
    fn text_blocks(&self) -> Vec<String> {
        self.iter()
            .filter_map(|block| match block {
                crate::ai::types::BlockContent::Text(text) => Some(text.text.clone()),
                crate::ai::types::BlockContent::Image(_) => None,
            })
            .collect()
    }
}

impl TextContentBlocks for &Vec<crate::ai::types::BlockContent> {
    fn text_blocks(&self) -> Vec<String> {
        self.as_slice().text_blocks()
    }
}

/// Port of `contentText`: extracts and joins text from message content.
pub fn content_text(content: impl TextContentBlocks, separator: &str) -> String {
    content.text_blocks().join(separator)
}

/// Port of `shortHash` (utils/hash.ts): a fast deterministic hash to shorten
/// long strings. Iterates UTF-16 code units so the digest matches the
/// TypeScript implementation for any input.
pub fn short_hash(value: &str) -> String {
    fn imul(a: u32, b: u32) -> u32 {
        a.wrapping_mul(b)
    }

    let mut h1: u32 = 0xdeadbeef;
    let mut h2: u32 = 0x41c6ce57;
    for unit in value.encode_utf16() {
        let ch = u32::from(unit);
        h1 = imul(h1 ^ ch, 2654435761);
        h2 = imul(h2 ^ ch, 1597334677);
    }
    h1 = imul(h1 ^ (h1 >> 16), 2246822507) ^ imul(h2 ^ (h2 >> 13), 3266489909);
    h2 = imul(h2 ^ (h2 >> 16), 2246822507) ^ imul(h1 ^ (h1 >> 13), 3266489909);
    format!("{}{}", to_base36(h2), to_base36(h1))
}

/// Formats a `u32` in base 36, matching JavaScript's `Number.prototype.toString(36)`.
fn to_base36(value: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    let mut value = value;
    while value > 0 {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base36 digits are ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::{
        AssistantContent, BlockContent, ImageContent, TextContent, ThinkingContent, ToolCall,
        UserContent,
    };

    /// Port of the `contentText` describe block in text.test.ts.
    #[test]
    fn content_text_extracts_assistant_text_blocks() {
        let content = vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "reasoning".to_string(),
                ..Default::default()
            }),
            AssistantContent::Text(TextContent {
                text: "first".to_string(),
                ..Default::default()
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "1".to_string(),
                name: "read".to_string(),
                ..Default::default()
            }),
            AssistantContent::Text(TextContent {
                text: "second".to_string(),
                ..Default::default()
            }),
        ];

        assert_eq!(content_text(&content, "\n"), "first\nsecond");
        assert_eq!(content_text(&content, ""), "firstsecond");
    }

    #[test]
    fn content_text_passes_string_content_through() {
        assert_eq!(content_text("hello", "\n"), "hello");
        assert_eq!(
            content_text(&UserContent::Text("hello".to_string()), "\n"),
            "hello"
        );
    }

    #[test]
    fn content_text_extracts_text_from_tool_result_content() {
        let content: Vec<BlockContent> = vec![
            BlockContent::Text(TextContent {
                text: "first".to_string(),
                ..Default::default()
            }),
            BlockContent::Image(ImageContent {
                content_type: crate::ai::types::TypeImage,
                data: "...".to_string(),
                mime_type: "image/png".to_string(),
            }),
            BlockContent::Text(TextContent {
                text: "second".to_string(),
                ..Default::default()
            }),
        ];

        assert_eq!(content_text(&content, ""), "firstsecond");
    }
}
