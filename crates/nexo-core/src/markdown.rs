//! Lightweight Markdown formatting and Emoji shortcode processor for chat messages.

#![allow(clippy::collapsible_if)]

/// Represents a parsed segment of formatted text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormattedSegment {
    Text(String),
    Bold(String),
    Italic(String),
    InlineCode(String),
    Link {
        label: String,
        url: String,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
}

/// Expands standard emoji shortcodes (e.g. `:smile:`, `:rocket:`, `:+1:`) to Unicode emojis.
#[must_use]
pub fn replace_emoji_shortcodes(input: &str) -> String {
    let mut output = input.to_owned();
    let mappings = [
        (":smile:", "😊"),
        (":happy:", "😄"),
        (":wave:", "👋"),
        (":heart:", "❤️"),
        (":+1:", "👍"),
        (":thumbsup:", "👍"),
        (":-1:", "👎"),
        (":thumbsdown:", "👎"),
        (":rocket:", "🚀"),
        (":fire:", "🔥"),
        (":check:", "✅"),
        (":lock:", "🔒"),
        (":shield:", "🛡️"),
        (":call:", "📞"),
        (":phone:", "📱"),
        (":mic:", "🎤"),
        (":camera:", "📷"),
        (":video:", "🎥"),
        (":pin:", "📌"),
        (":warning:", "⚠️"),
        (":sparkles:", "✨"),
        (":tada:", "🎉"),
        (":eyes:", "👀"),
    ];

    for (shortcode, emoji) in mappings {
        output = output.replace(shortcode, emoji);
    }
    output
}

/// Parses message text into a sequence of formatted markdown segments.
#[must_use]
pub fn parse_markdown(input: &str) -> Vec<FormattedSegment> {
    let text = replace_emoji_shortcodes(input);
    let mut segments = Vec::new();

    // Check for fenced code block ```...```
    if text.starts_with("```") && text.ends_with("```") && text.len() >= 6 {
        let inner = &text[3..text.len() - 3];
        let mut lines = inner.lines();
        let first_line = lines.next().unwrap_or("").trim();
        let lang = if first_line.is_empty() || inner.starts_with('\n') {
            None
        } else {
            Some(first_line.to_owned())
        };
        let code = if lang.is_some() {
            lines.collect::<Vec<_>>().join("\n")
        } else {
            inner.trim_start_matches('\n').to_owned()
        };
        segments.push(FormattedSegment::CodeBlock {
            language: lang,
            code,
        });
        return segments;
    }

    let mut cursor = 0;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

    while cursor < len {
        // 1. Inline code: `...`
        if chars[cursor] == '`' {
            if let Some(end) = chars[cursor + 1..].iter().position(|&c| c == '`') {
                let code_content: String = chars[cursor + 1..cursor + 1 + end].iter().collect();
                segments.push(FormattedSegment::InlineCode(code_content));
                cursor += end + 2;
                continue;
            }
        }

        // 2. Bold: **...**
        if cursor + 1 < len && chars[cursor] == '*' && chars[cursor + 1] == '*' {
            let search_start = cursor + 2;
            let mut end_idx = None;
            for i in search_start..len - 1 {
                if chars[i] == '*' && chars[i + 1] == '*' {
                    end_idx = Some(i);
                    break;
                }
            }
            if let Some(end) = end_idx {
                let bold_content: String = chars[search_start..end].iter().collect();
                segments.push(FormattedSegment::Bold(bold_content));
                cursor = end + 2;
                continue;
            }
        }

        // 3. Italic: *...*
        if chars[cursor] == '*' {
            let search_start = cursor + 1;
            if let Some(pos) = chars[search_start..].iter().position(|&c| c == '*') {
                let italic_content: String =
                    chars[search_start..search_start + pos].iter().collect();
                segments.push(FormattedSegment::Italic(italic_content));
                cursor += pos + 2;
                continue;
            }
        }

        // 4. URL Link detection: https://... or http://...
        let remaining: String = chars[cursor..].iter().collect();
        if remaining.starts_with("https://") || remaining.starts_with("http://") {
            let end = remaining
                .find(|c: char| c.is_whitespace() || c == ')' || c == ']' || c == '>')
                .unwrap_or(remaining.len());
            let url = remaining[..end].to_owned();
            segments.push(FormattedSegment::Link {
                label: url.clone(),
                url,
            });
            cursor += end;
            continue;
        }

        // Accumulate plain text until next token
        let mut text_end = cursor + 1;
        while text_end < len {
            let c = chars[text_end];
            if c == '`' || c == '*' || (c == 'h' && text[text_end..].starts_with("http")) {
                break;
            }
            text_end += 1;
        }

        let plain: String = chars[cursor..text_end].iter().collect();
        segments.push(FormattedSegment::Text(plain));
        cursor = text_end;
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_shortcodes_are_replaced() {
        assert_eq!(
            replace_emoji_shortcodes("Ola :wave: :rocket:!"),
            "Ola 👋 🚀!"
        );
        assert_eq!(replace_emoji_shortcodes("Privado :lock:"), "Privado 🔒");
    }

    #[test]
    fn parses_bold_italic_and_inline_code() {
        let input = "Ola **amigo**, veja `nexo.exe` e *seguro* :rocket:";
        let segments = parse_markdown(input);

        assert_eq!(
            segments,
            vec![
                FormattedSegment::Text("Ola ".into()),
                FormattedSegment::Bold("amigo".into()),
                FormattedSegment::Text(", veja ".into()),
                FormattedSegment::InlineCode("nexo.exe".into()),
                FormattedSegment::Text(" e ".into()),
                FormattedSegment::Italic("seguro".into()),
                FormattedSegment::Text(" 🚀".into()),
            ]
        );
    }

    #[test]
    fn parses_code_block() {
        let input = "```rust\nfn main() {}\n```";
        let segments = parse_markdown(input);

        assert_eq!(
            segments,
            vec![FormattedSegment::CodeBlock {
                language: Some("rust".into()),
                code: "fn main() {}".into(),
            }]
        );
    }

    #[test]
    fn parses_hyperlinks() {
        let input = "Visite https://github.com/VicRyan007/Nexo para novidades!";
        let segments = parse_markdown(input);

        assert_eq!(
            segments,
            vec![
                FormattedSegment::Text("Visite ".into()),
                FormattedSegment::Link {
                    label: "https://github.com/VicRyan007/Nexo".into(),
                    url: "https://github.com/VicRyan007/Nexo".into(),
                },
                FormattedSegment::Text(" para novidades!".into()),
            ]
        );
    }
}
