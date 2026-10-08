//! Port of `pi-core/ai/src/utils/json-parse.ts`: JSON repair for malformed
//! string literals and lenient parsing of incomplete JSON during streaming.
//!
//! The TypeScript version delegates incomplete JSON to the `partial-json`
//! npm package. The Rust port embeds an equivalent lenient parser with the
//! same observable behavior used by the providers: incomplete strings are
//! closed, missing array/object closers are appended, and incomplete trailing
//! object keys or array elements are dropped.

/// Valid JSON escape characters after a backslash.
const VALID_JSON_ESCAPES: [char; 9] = ['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

fn is_control_character(ch: char) -> bool {
    (ch as u32) <= 0x1f
}

fn escape_control_character(ch: char) -> String {
    match ch {
        '\u{8}' => "\\b".to_string(),
        '\u{c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        _ => format!("\\u{:04x}", ch as u32),
    }
}

/// Port of `repairJson`: repairs malformed JSON string literals by escaping
/// raw control characters inside strings and doubling backslashes before
/// invalid escape characters.
pub fn repair_json(json: &str) -> String {
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;

    let chars: Vec<char> = json.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];

        if !in_string {
            repaired.push(ch);
            if ch == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if ch == '"' {
            repaired.push(ch);
            in_string = false;
            index += 1;
            continue;
        }

        if ch == '\\' {
            let next = chars.get(index + 1).copied();
            match next {
                None => {
                    repaired.push_str("\\\\");
                }
                Some('u') => {
                    let digits: String = chars[index + 2..(index + 6).min(chars.len())]
                        .iter()
                        .collect();
                    if chars.len() >= index + 6
                        && digits.len() == 4
                        && digits.chars().all(|c| c.is_ascii_hexdigit())
                    {
                        repaired.push_str(&format!("\\u{digits}"));
                        // Consumed backslash + 'u' + 4 digits; the block's
                        // trailing increment accounts for the last digit.
                        index += 5;
                    } else {
                        repaired.push_str("\\\\");
                    }
                }
                Some(next) if VALID_JSON_ESCAPES.contains(&next) => {
                    repaired.push('\\');
                    repaired.push(next);
                    index += 1;
                }
                Some(_) => {
                    repaired.push_str("\\\\");
                }
            }
            index += 1;
            continue;
        }

        if is_control_character(ch) {
            repaired.push_str(&escape_control_character(ch));
        } else {
            repaired.push(ch);
        }
        index += 1;
    }

    repaired
}

/// Port of `parseJsonWithRepair`: parses JSON, retrying once against a
/// repaired copy when strict parsing fails.
pub fn parse_json_with_repair<T: serde::de::DeserializeOwned>(
    json: &str,
) -> Result<T, serde_json::Error> {
    match serde_json::from_str(json) {
        Ok(value) => Ok(value),
        Err(original) => {
            let repaired = repair_json(json);
            if repaired != json {
                serde_json::from_str(&repaired)
            } else {
                Err(original)
            }
        }
    }
}

/// Lenient parse outcome: the parse stopped because input ended (the value so
/// far is recoverable) or the input was malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartialError {
    /// Input ended where more characters were required.
    Eof,
    /// Input contained invalid JSON.
    Invalid,
}

type PartialResult = Result<serde_json::Value, PartialError>;

struct PartialParser<'a> {
    chars: Vec<char>,
    position: usize,
    _input: &'a str,
}

/// Validates a token against strict JSON number grammar (Rust's `f64` parser
/// is more lenient, accepting e.g. `1.`).
fn is_strict_json_number(text: &str) -> bool {
    serde_json::from_str::<serde_json::Number>(text).is_ok()
}

/// Parses a numeric token with JavaScript `Number` semantics: integral
/// values (including exponent forms like `1e5`) serialize as integers.
fn js_canonical_number(text: &str) -> Option<serde_json::Value> {
    if let Ok(int) = text.parse::<i64>() {
        return Some(serde_json::Value::Number(int.into()));
    }
    if let Ok(uint) = text.parse::<u64>() {
        return Some(serde_json::Value::Number(uint.into()));
    }
    let float = text.parse::<f64>().ok()?;
    if float.is_finite() && float.fract() == 0.0 && float.abs() <= 9_007_199_254_740_991.0 {
        return Some(serde_json::Value::Number((float as i64).into()));
    }
    serde_json::Number::from_f64(float).map(serde_json::Value::Number)
}

/// Parses potentially incomplete JSON leniently, mirroring the `partial-json`
/// behavior relied on by the providers. Returns `None` when the input is
/// malformed (not merely incomplete).
pub fn parse_partial_json(json: &str) -> Option<serde_json::Value> {
    let mut parser = PartialParser {
        chars: json.chars().collect(),
        position: 0,
        _input: json,
    };
    // Trailing content after the first complete value is ignored, matching
    // the `partial-json` package.
    parser.parse_value().ok()
}

impl<'a> PartialParser<'a> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.position += 1;
        }
        ch
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.peek(),
            Some(' ') | Some('\t') | Some('\n') | Some('\r')
        ) {
            self.position += 1;
        }
    }

    fn parse_value(&mut self) -> PartialResult {
        self.skip_whitespace();
        match self.peek() {
            None => Err(PartialError::Eof),
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => self.parse_string().map(serde_json::Value::String),
            Some('t') => self.parse_keyword("true", serde_json::Value::Bool(true)),
            Some('f') => self.parse_keyword("false", serde_json::Value::Bool(false)),
            Some('n') => self.parse_keyword("null", serde_json::Value::Null),
            Some(_) => self.parse_number(),
        }
    }

    fn parse_keyword(&mut self, keyword: &str, value: serde_json::Value) -> PartialResult {
        let remaining: String = self.chars[self.position..]
            .iter()
            .take(keyword.len())
            .collect();
        if remaining == keyword {
            self.position += keyword.len();
            Ok(value)
        } else if keyword.starts_with(&remaining) {
            // Incomplete keywords resolve optimistically during streaming
            // ("tru" -> true), matching `partial-json`.
            self.position = self.chars.len();
            Ok(value)
        } else {
            Err(PartialError::Invalid)
        }
    }

    fn parse_string(&mut self) -> Result<String, PartialError> {
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            match self.bump() {
                None => {
                    // Incomplete string: the accumulated text is kept with
                    // the implicit closing quote.
                    return Ok(out);
                }
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    None => {
                        // Input ends right after a backslash; `partial-json`
                        // keeps it as a literal backslash.
                        out.push('\\');
                        return Ok(out);
                    }
                    Some(escape) => match escape {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let hex: String = self.chars
                                [self.position..(self.position + 4).min(self.chars.len())]
                                .iter()
                                .collect();
                            if hex.chars().count() == 4
                                && hex.chars().all(|c| c.is_ascii_hexdigit())
                            {
                                self.position += 4;
                                let code = u32::from_str_radix(&hex, 16).unwrap_or(0xfffd);
                                // Handle surrogate pairs; unpaired surrogates
                                // become the replacement character.
                                if (0xd800..0xdc00).contains(&code) {
                                    if self.chars.get(self.position).copied() == Some('\\')
                                        && self.chars.get(self.position + 1).copied() == Some('u')
                                    {
                                        let low: String = self.chars[self.position + 2
                                            ..(self.position + 6).min(self.chars.len())]
                                            .iter()
                                            .collect();
                                        if low.chars().count() == 4
                                            && low.chars().all(|c| c.is_ascii_hexdigit())
                                        {
                                            let low_code =
                                                u32::from_str_radix(&low, 16).unwrap_or(0xfffd);
                                            if (0xdc00..0xe000).contains(&low_code) {
                                                self.position += 6;
                                                let combined = 0x10000
                                                    + ((code - 0xd800) << 10)
                                                    + (low_code - 0xdc00);
                                                out.push(
                                                    char::from_u32(combined).unwrap_or('\u{fffd}'),
                                                );
                                                continue;
                                            }
                                        }
                                    }
                                    out.push('\u{fffd}');
                                } else {
                                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                }
                            } else {
                                // Incomplete or invalid unicode escape; stop
                                // the string here.
                                return Ok(out);
                            }
                        }
                        _ => return Err(PartialError::Invalid),
                    },
                },
                Some(ch) if is_control_character(ch) => return Err(PartialError::Invalid),
                Some(ch) => out.push(ch),
            }
        }
    }

    fn parse_number(&mut self) -> PartialResult {
        let start = self.position;
        while matches!(
            self.peek(),
            Some('0'..='9') | Some('-') | Some('+') | Some('.') | Some('e') | Some('E')
        ) {
            self.position += 1;
        }
        let text: String = self.chars[start..self.position].iter().collect();
        if text.is_empty() {
            return Err(PartialError::Invalid);
        }
        if is_strict_json_number(&text)
            && let Some(value) = js_canonical_number(&text)
        {
            return Ok(value);
        }
        // A token truncated mid-exponent ("1e", "1.5e-") drops the dangling
        // exponent and parses the mantissa; other truncated numbers ("1.",
        // "-", "01") are dropped by the caller, matching `partial-json`.
        let at_eof = self.position >= self.chars.len();
        let mantissa = text
            .trim_end_matches(['+', '-'])
            .trim_end_matches(['e', 'E'])
            .trim_end_matches(['+', '-']);
        if at_eof
            && mantissa != text
            && is_strict_json_number(mantissa)
            && let Some(value) = js_canonical_number(mantissa)
        {
            return Ok(value);
        }
        if at_eof {
            Err(PartialError::Eof)
        } else {
            Err(PartialError::Invalid)
        }
    }

    fn parse_object(&mut self) -> PartialResult {
        self.bump(); // '{'
        let mut map = serde_json::Map::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None => return Ok(serde_json::Value::Object(map)),
                Some('}') => {
                    self.bump();
                    return Ok(serde_json::Value::Object(map));
                }
                Some('"') => {}
                // Anything unparseable at a member position drops the rest,
                // matching `partial-json`'s degradation.
                Some(_) => return Ok(serde_json::Value::Object(map)),
            }

            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.peek() != Some(':') {
                // Missing colon: the incomplete pair is dropped.
                return Ok(serde_json::Value::Object(map));
            }
            self.bump();

            self.skip_whitespace();
            let value = match self.parse_value() {
                Ok(value) => value,
                Err(_) => return Ok(serde_json::Value::Object(map)),
            };
            map.insert(key, value);

            self.skip_whitespace();
            match self.peek() {
                None => return Ok(serde_json::Value::Object(map)),
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    return Ok(serde_json::Value::Object(map));
                }
                // Trailing garbage after a member is ignored.
                Some(_) => return Ok(serde_json::Value::Object(map)),
            }
        }
    }

    fn parse_array(&mut self) -> PartialResult {
        self.bump(); // '['
        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None => return Ok(serde_json::Value::Array(items)),
                Some(']') => {
                    self.bump();
                    return Ok(serde_json::Value::Array(items));
                }
                // Anything unparseable at an element position drops the rest,
                // matching `partial-json`'s degradation.
                Some(_) => {}
            }

            let value = match self.parse_value() {
                Ok(value) => value,
                Err(_) => return Ok(serde_json::Value::Array(items)),
            };
            items.push(value);

            self.skip_whitespace();
            match self.peek() {
                None => return Ok(serde_json::Value::Array(items)),
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    return Ok(serde_json::Value::Array(items));
                }
                // Trailing garbage after an element is ignored.
                Some(_) => return Ok(serde_json::Value::Array(items)),
            }
        }
    }
}

/// Port of `parseStreamingJson`: attempts strict parsing, then repair, then
/// lenient partial parsing; always resolves to a valid JSON value (`null`
/// when nothing could be parsed).
pub fn parse_streaming_json(partial_json: Option<&str>) -> serde_json::Value {
    let Some(partial_json) = partial_json else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    if partial_json.trim().is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }

    if let Ok(value) = parse_json_with_repair::<serde_json::Value>(partial_json) {
        return value;
    }
    if let Some(value) = parse_partial_json(partial_json) {
        return value;
    }
    if let Some(value) = parse_partial_json(&repair_json(partial_json)) {
        return value;
    }
    serde_json::Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Behaviors pinned against the `partial-json` npm package used by the
    /// TypeScript implementation.
    #[test]
    fn matches_partial_json_semantics() {
        let cases: Vec<(&str, serde_json::Value)> = vec![
            (r#"{"path": "/tmp/lo"#, json!({"path": "/tmp/lo"})),
            (r#"{"a": 1, "b""#, json!({"a": 1})),
            (r#"{"a": 1, "b":"#, json!({"a": 1})),
            (r#"{"a": [1, 2"#, json!({"a": [1, 2]})),
            ("{", json!({})),
            (r#"[1, "ab"#, json!([1, "ab"])),
            ("tru", json!(true)),
            (r#"{"a": 1, "#, json!({"a": 1})),
            (r#"{"a": {"b": "c"#, json!({"a": {"b": "c"}})),
            ("123", json!(123)),
            (r#"{"a":"#, json!({})),
            (r#"{"a": tru"#, json!({"a": true})),
            (r#"{"a": 1}}"#, json!({"a": 1})),
            (r#"{"a": 1,}"#, json!({"a": 1})),
            ("null", serde_json::Value::Null),
            (r#"{"a": 1."#, json!({})),
            (r#"{"a": 1e"#, json!({"a": 1})),
            (r#"{"a": 1, "b": tru"#, json!({"a": 1, "b": true})),
            ("[1, 2,", json!([1, 2])),
            ("[1,", json!([1])),
            ("[", json!([])),
            (r#"{"a": "b"#, json!({"a": "b"})),
            (r#"{"a": "b\\""#, json!({"a": "b\\"})),
            (r#"{"a" 1}"#, json!({})),
            ("{a: 1}", json!({})),
            (r#"{"a": nul"#, json!({"a": null})),
            (r#"{"a": fals"#, json!({"a": false})),
            (r#"{"a": -"#, json!({})),
            (r#"{"a": 1.5"#, json!({"a": 1.5})),
            (r#""abc"#, json!("abc")),
            (r#"{"a": 12."#, json!({})),
            (r#"{"a": 1.5e"#, json!({"a": 1.5})),
            (r#"{"a": 01"#, json!({})),
            (r#"{"a": 1e-"#, json!({"a": 1})),
            (r#"{"a": 1e5"#, json!({"a": 100000})),
            (r#"{"a": truex"#, json!({"a": true})),
            (r#"{"a": truz"#, json!({})),
            (
                r#"{"a": "b", "c": [1, {"d": "e"#,
                json!({"a": "b", "c": [1, {"d": "e"}]}),
            ),
            (r#"{"a": "x\n"#, json!({"a": "x\n"})),
        ];

        for (input, expected) in cases {
            let parsed = parse_partial_json(input)
                .unwrap_or_else(|| panic!("expected partial parse of {input}"));
            assert_eq!(parsed, expected, "partial parse of {input}");
        }

        // Whitespace-only and empty inputs fail strict partial parsing (the
        // caller falls back to an empty object).
        assert_eq!(parse_partial_json("   "), None);
        assert_eq!(parse_partial_json(""), None);
    }

    #[test]
    fn repair_json_escapes_control_characters_and_backslashes() {
        assert_eq!(repair_json("{\"a\": \"x\ty\"}"), "{\"a\": \"x\\ty\"}");
        assert_eq!(repair_json("{\"a\": \"x\\\\y\"}"), "{\"a\": \"x\\\\y\"}");
        // Backslashes outside string literals are copied as-is, like the
        // TypeScript implementation.
        assert_eq!(repair_json("back\\slash"), "back\\slash");
        assert_eq!(repair_json("trailing\\"), "trailing\\");
        // Invalid escape characters get the backslash doubled.
        assert_eq!(repair_json("{\"a\": \"\\x\"}"), "{\"a\": \"\\\\x\"}");
        // Valid escapes and \\uXXXX pass through.
        assert_eq!(repair_json("{\"a\": \"\\u0041\"}"), "{\"a\": \"\\u0041\"}");
        assert_eq!(repair_json("{\"a\": \"\\n\"}"), "{\"a\": \"\\n\"}");
    }

    #[test]
    fn parse_streaming_json_falls_back_to_empty_object() {
        assert_eq!(parse_streaming_json(None), json!({}));
        assert_eq!(parse_streaming_json(Some("")), json!({}));
        assert_eq!(parse_streaming_json(Some("  ")), json!({}));
        assert_eq!(parse_streaming_json(Some("@@")), json!({}));
    }

    #[test]
    fn parse_streaming_json_prefers_strict_and_repair_paths() {
        assert_eq!(parse_streaming_json(Some(r#"{"a": 1}"#)), json!({"a": 1}));
        // Raw control characters: strict parse fails, repair fixes it.
        assert_eq!(
            parse_streaming_json(Some("{\"a\": \"x\ty\"}")),
            json!({"a": "x\ty"})
        );
    }
}
