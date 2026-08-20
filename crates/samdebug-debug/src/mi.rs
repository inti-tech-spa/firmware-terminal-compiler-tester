use samdebug_core::{ErrorCategory, SamdebugError, SamdebugResult};

const MAX_MI_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiRecord {
    Result {
        token: Option<u64>,
        class: String,
        results: Vec<MiResult>,
    },
    Exec {
        class: String,
        results: Vec<MiResult>,
    },
    Status {
        class: String,
        results: Vec<MiResult>,
    },
    Notify {
        class: String,
        results: Vec<MiResult>,
    },
    Console(String),
    Target(String),
    Log(String),
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiResult {
    pub variable: String,
    pub value: MiValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiValue {
    Const(String),
    Tuple(Vec<MiResult>),
    List(Vec<MiListItem>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiListItem {
    Value(MiValue),
    Result(MiResult),
}

#[derive(Debug, Default)]
pub struct MiStreamParser {
    pending: Vec<u8>,
}

impl MiStreamParser {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> SamdebugResult<Vec<MiRecord>> {
        self.pending.extend_from_slice(bytes);
        let mut records = Vec::new();
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            if index > MAX_MI_RECORD_BYTES {
                self.pending.clear();
                return Err(mi_error(
                    "MI_RECORD_TOO_LARGE",
                    "GDB/MI record exceeds 1 MiB",
                ));
            }
            let mut line = self.pending.drain(..=index).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let text = std::str::from_utf8(&line)
                .map_err(|error| mi_error("MI_UTF8_INVALID", error.to_string()))?;
            records.push(parse_record(text)?);
        }
        if self.pending.len() > MAX_MI_RECORD_BYTES {
            self.pending.clear();
            return Err(mi_error(
                "MI_RECORD_TOO_LARGE",
                "GDB/MI record exceeds 1 MiB",
            ));
        }
        Ok(records)
    }

    pub fn finish(&mut self) -> SamdebugResult<()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            self.pending.clear();
            Err(mi_error(
                "MI_RECORD_TRUNCATED",
                "GDB output ended with a partial MI record",
            ))
        }
    }
}

fn parse_record(text: &str) -> SamdebugResult<MiRecord> {
    if text == "(gdb)" || text == "(gdb) " {
        return Ok(MiRecord::Prompt);
    }
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    let token = if cursor == 0 {
        None
    } else {
        Some(
            text[..cursor]
                .parse::<u64>()
                .map_err(|error| mi_error("MI_TOKEN_INVALID", error.to_string()))?,
        )
    };
    let prefix = *bytes
        .get(cursor)
        .ok_or_else(|| mi_error("MI_RECORD_INVALID", "missing record prefix"))?;
    cursor += 1;
    if matches!(prefix, b'~' | b'@' | b'&') {
        if token.is_some() {
            return Err(mi_error("MI_RECORD_INVALID", "stream record has a token"));
        }
        let mut parser = ValueParser::new(&text[cursor..]);
        let value = parser.parse_c_string()?;
        parser.require_end()?;
        return Ok(match prefix {
            b'~' => MiRecord::Console(value),
            b'@' => MiRecord::Target(value),
            b'&' => MiRecord::Log(value),
            _ => unreachable!(),
        });
    }
    if !matches!(prefix, b'^' | b'*' | b'+' | b'=') {
        return Err(mi_error(
            "MI_RECORD_INVALID",
            format!("unknown record prefix {}", char::from(prefix)),
        ));
    }
    let remainder = &text[cursor..];
    let class_end = remainder.find(',').unwrap_or(remainder.len());
    let class = &remainder[..class_end];
    if class.is_empty() || !class.bytes().all(is_identifier_byte) {
        return Err(mi_error("MI_RECORD_INVALID", "invalid result class"));
    }
    let results = if class_end == remainder.len() {
        Vec::new()
    } else {
        let mut parser = ValueParser::new(&remainder[class_end + 1..]);
        // GDB's documented download progress extension is emitted as
        // `+download,{section=...,section-size=...,total-size=...}`. Unlike
        // normal async output, its top-level tuple has no result variable.
        // Accept only that exact status class and give the tuple a stable
        // internal name so the rest of the engine remains strongly typed.
        let results = if prefix == b'+' && class == "download" && parser.peek() == Some(b'{') {
            vec![MiResult {
                variable: "progress".into(),
                value: parser.parse_value()?,
            }]
        } else {
            parser.parse_result_sequence(None)?
        };
        parser.require_end()?;
        results
    };
    let class = class.to_owned();
    Ok(match prefix {
        b'^' => MiRecord::Result {
            token,
            class,
            results,
        },
        b'*' => MiRecord::Exec { class, results },
        b'+' => MiRecord::Status { class, results },
        b'=' => MiRecord::Notify { class, results },
        _ => unreachable!(),
    })
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

#[derive(Debug)]
struct ValueParser<'a> {
    text: &'a str,
    cursor: usize,
}

impl<'a> ValueParser<'a> {
    const fn new(text: &'a str) -> Self {
        Self { text, cursor: 0 }
    }

    fn require_end(&self) -> SamdebugResult<()> {
        if self.cursor == self.text.len() {
            Ok(())
        } else {
            Err(mi_error("MI_RECORD_INVALID", "unexpected trailing input"))
        }
    }

    fn parse_result_sequence(&mut self, terminator: Option<u8>) -> SamdebugResult<Vec<MiResult>> {
        let mut results = Vec::new();
        if terminator.is_some_and(|value| self.peek() == Some(value)) {
            return Ok(results);
        }
        loop {
            results.push(self.parse_result()?);
            match self.peek() {
                Some(b',') => self.cursor += 1,
                value if value == terminator || value.is_none() => break,
                _ => {
                    return Err(mi_error(
                        "MI_RECORD_INVALID",
                        "expected comma or terminator",
                    ));
                }
            }
        }
        Ok(results)
    }

    fn parse_result(&mut self) -> SamdebugResult<MiResult> {
        let start = self.cursor;
        while self.peek().is_some_and(is_identifier_byte) {
            self.cursor += 1;
        }
        if self.cursor == start || self.peek() != Some(b'=') {
            return Err(mi_error("MI_RECORD_INVALID", "invalid result variable"));
        }
        let variable = self.text[start..self.cursor].to_owned();
        self.cursor += 1;
        Ok(MiResult {
            variable,
            value: self.parse_value()?,
        })
    }

    fn parse_value(&mut self) -> SamdebugResult<MiValue> {
        match self.peek() {
            Some(b'"') => Ok(MiValue::Const(self.parse_c_string()?)),
            Some(b'{') => {
                self.cursor += 1;
                let values = self.parse_result_sequence(Some(b'}'))?;
                self.expect(b'}')?;
                Ok(MiValue::Tuple(values))
            }
            Some(b'[') => self.parse_list(),
            _ => Err(mi_error("MI_RECORD_INVALID", "invalid MI value")),
        }
    }

    fn parse_list(&mut self) -> SamdebugResult<MiValue> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.cursor += 1;
            return Ok(MiValue::List(items));
        }
        loop {
            let saved = self.cursor;
            while self.peek().is_some_and(is_identifier_byte) {
                self.cursor += 1;
            }
            let is_result = self.cursor > saved && self.peek() == Some(b'=');
            self.cursor = saved;
            items.push(if is_result {
                MiListItem::Result(self.parse_result()?)
            } else {
                MiListItem::Value(self.parse_value()?)
            });
            match self.peek() {
                Some(b',') => self.cursor += 1,
                Some(b']') => {
                    self.cursor += 1;
                    break;
                }
                _ => return Err(mi_error("MI_RECORD_INVALID", "invalid MI list")),
            }
        }
        Ok(MiValue::List(items))
    }

    fn parse_c_string(&mut self) -> SamdebugResult<String> {
        self.expect(b'"')?;
        let mut bytes = Vec::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| mi_error("MI_RECORD_INVALID", "unterminated C string"))?;
            match byte {
                b'"' => break,
                b'\\' => self.parse_escape(&mut bytes)?,
                value => bytes.push(value),
            }
        }
        String::from_utf8(bytes).map_err(|error| mi_error("MI_UTF8_INVALID", error.to_string()))
    }

    fn parse_escape(&mut self, bytes: &mut Vec<u8>) -> SamdebugResult<()> {
        let escaped = self
            .next()
            .ok_or_else(|| mi_error("MI_RECORD_INVALID", "incomplete C escape"))?;
        match escaped {
            b'n' => bytes.push(b'\n'),
            b'r' => bytes.push(b'\r'),
            b't' => bytes.push(b'\t'),
            b'b' => bytes.push(8),
            b'f' => bytes.push(12),
            b'v' => bytes.push(11),
            b'a' => bytes.push(7),
            b'"' | b'\\' => bytes.push(escaped),
            b'0'..=b'7' => {
                let mut value = escaped - b'0';
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = self.peek() {
                        self.cursor += 1;
                        value = value.saturating_mul(8).saturating_add(next - b'0');
                    } else {
                        break;
                    }
                }
                bytes.push(value);
            }
            _ => return Err(mi_error("MI_RECORD_INVALID", "unsupported C escape")),
        }
        Ok(())
    }

    fn expect(&mut self, byte: u8) -> SamdebugResult<()> {
        if self.next() == Some(byte) {
            Ok(())
        } else {
            Err(mi_error("MI_RECORD_INVALID", "unexpected character"))
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.cursor).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let value = self.peek()?;
        self.cursor += 1;
        Some(value)
    }
}

fn mi_error(code: &str, message: impl Into<String>) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Debugger, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fragmented_result_async_and_stream_records() {
        let mut parser = MiStreamParser::new();
        assert!(
            parser
                .push(b"12^done,bkpt={number=\"1\",")
                .unwrap()
                .is_empty()
        );
        let records = parser
            .push(
                b"addr=\"0x00400100\"}\n*stopped,reason=\"breakpoint-hit\"\n@\"hello\\n\"\n(gdb)\n",
            )
            .expect("records");
        assert_eq!(records.len(), 4);
        assert!(matches!(
            &records[0],
            MiRecord::Result { token: Some(12), class, .. } if class == "done"
        ));
        assert!(matches!(
            &records[1],
            MiRecord::Exec { class, .. } if class == "stopped"
        ));
        assert_eq!(records[2], MiRecord::Target("hello\n".into()));
        assert_eq!(records[3], MiRecord::Prompt);
    }

    #[test]
    fn parses_lists_tuples_results_and_c_escapes() {
        let mut parser = MiStreamParser::new();
        let records = parser
            .push(
                b"3^done,values=[{name=\"r0\",value=\"0x1\"},name=\"x\"],msg=\"a\\tb\\\\c\\042\"\n",
            )
            .expect("record");
        let MiRecord::Result { results, .. } = &records[0] else {
            panic!("result record")
        };
        assert!(matches!(results[0].value, MiValue::List(_)));
        assert_eq!(results[1].value, MiValue::Const("a\tb\\c\"".into()));
    }

    #[test]
    fn parses_gdb_download_progress_anonymous_tuple_only_for_download_status() {
        let mut parser = MiStreamParser::new();
        let records = parser
            .push(b"+download,{section=\".text\",section-size=\"256\",total-size=\"4096\"}\n")
            .expect("download progress");
        let MiRecord::Status { class, results } = &records[0] else {
            panic!("status record")
        };
        assert_eq!(class, "download");
        assert_eq!(results[0].variable, "progress");
        assert!(matches!(results[0].value, MiValue::Tuple(_)));

        assert_eq!(
            parser
                .push(b"+other,{name=\"not-allowed\"}\n")
                .unwrap_err()
                .code(),
            "MI_RECORD_INVALID"
        );
    }

    #[test]
    fn rejects_malformed_and_oversized_records_without_panicking() {
        let mut parser = MiStreamParser::new();
        assert_eq!(
            parser.push(b"^done,x=\"bad\\q\"\n").unwrap_err().code(),
            "MI_RECORD_INVALID"
        );
        let mut parser = MiStreamParser::new();
        assert_eq!(
            parser
                .push(&vec![b'x'; MAX_MI_RECORD_BYTES + 1])
                .unwrap_err()
                .code(),
            "MI_RECORD_TOO_LARGE"
        );
        let mut parser = MiStreamParser::new();
        let mut complete = vec![b'x'; MAX_MI_RECORD_BYTES + 1];
        complete.push(b'\n');
        assert_eq!(
            parser.push(&complete).unwrap_err().code(),
            "MI_RECORD_TOO_LARGE"
        );
        let mut parser = MiStreamParser::new();
        parser.push(b"^do").expect("fragment");
        assert_eq!(parser.finish().unwrap_err().code(), "MI_RECORD_TRUNCATED");
    }
}
