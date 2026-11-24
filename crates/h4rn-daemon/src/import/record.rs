//! One-record-at-a-time JSON streaming for the two transcript formats.
//!
//! A transcript is JSON Lines, but a single line can be far larger than the
//! text worth keeping: an attachment or a tool result may hold megabytes that
//! the import never reads. Buffering such a line would either waste memory or
//! force a size cap that silently drops the prompt next to it.
//!
//! [`RecordReader`] exposes the current line as a [`Read`] stream and never
//! holds more than one read-ahead chunk, and the record types in [`claude`]
//! and [`codex`] name only the fields the import uses. Every other field,
//! including a multi-megabyte attachment, is skipped by the JSON deserialiser
//! without being allocated. A malformed line is discarded to its newline and
//! the next record is read normally.
//!
//! [`claude`]: super::claude
//! [`codex`]: super::codex

use std::io::{self, BufRead, Read};

use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, SeqAccess, Visitor};

/// What one line of a transcript yielded.
pub(crate) enum Record<T> {
    /// The line parsed.
    Parsed(T),
    /// The line was empty or only whitespace.
    Blank,
    /// The line could not be parsed and was skipped.
    Malformed,
    /// The line was longer than [`MAX_RECORD_BYTES`] and was skipped.
    ///
    /// This is reported apart from [`Record::Malformed`]: the line may well
    /// have been valid, and the conversation it belongs to cannot be kept
    /// whole, so the importer warns instead of dropping it silently.
    OverLimit,
    /// The file ended.
    End,
}

/// Reads JSON Lines one record at a time with bounded memory.
pub(crate) struct RecordReader<R> {
    inner: R,
    /// Bytes of the current line that were read ahead of the deserialiser.
    pending: Vec<u8>,
    /// How much of `pending` the deserialiser has taken.
    position: usize,
    /// The current line's newline has been consumed.
    line_ended: bool,
    /// The whole file has been read.
    eof: bool,
    /// Whether the current line had any bytes at all.
    started: bool,
    /// Bytes in the current line, including what the deserialiser read.
    bytes: u64,
    /// A line longer than the reader was asked to keep.
    over_limit: bool,
}

/// How many bytes of one line a record reader will feed the deserialiser.
///
/// Text the import keeps is capped at 32 MiB per conversation; a record that
/// still has not ended after this much is a malformed or pathological line, and
/// the rest of it is skipped to the newline so the file can continue.
const MAX_RECORD_BYTES: u64 = 48 * 1024 * 1024;

impl<R: BufRead> RecordReader<R> {
    /// Wraps a buffered reader positioned at the start of a line.
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            position: 0,
            line_ended: false,
            eof: false,
            started: false,
            bytes: 0,
            over_limit: false,
        }
    }

    /// Reads and parses the next line.
    pub(crate) fn next<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Record<T>> {
        let mut deserializer = serde_json::Deserializer::from_reader(&mut *self);
        let parsed = T::deserialize(&mut deserializer).and_then(|value| {
            deserializer.end()?;
            Ok(value)
        });
        let content = self.finish_line()?;
        Ok(match content {
            Line::End => Record::End,
            Line::Blank => Record::Blank,
            Line::OverLimit => Record::OverLimit,
            Line::Malformed => Record::Malformed,
            Line::Readable => match parsed {
                Ok(value) => Record::Parsed(value),
                Err(_) => Record::Malformed,
            },
        })
    }

    /// Discards what is left of the current line and moves to the next.
    fn finish_line(&mut self) -> io::Result<Line> {
        let has_bytes = self.started;
        // Read-ahead the deserialiser did not consume belongs to this line.
        if self.position < self.pending.len() {
            self.started = true;
        }
        self.pending.clear();
        self.position = 0;
        if !self.line_ended && !self.eof {
            loop {
                let available = self.inner.fill_buf()?;
                if available.is_empty() {
                    self.eof = true;
                    break;
                }
                if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                    self.started |= newline > 0;
                    self.inner.consume(newline + 1);
                    break;
                }
                let length = available.len();
                self.started = true;
                self.inner.consume(length);
            }
        }
        let readable = self.started && !self.over_limit;
        let over_limit = self.over_limit;
        let blank = !self.started;
        self.line_ended = false;
        self.started = false;
        self.bytes = 0;
        self.over_limit = false;
        Ok(if self.eof && !readable && !has_bytes && blank {
            // Nothing was read and the file is exhausted.
            Line::End
        } else if blank {
            Line::Blank
        } else if over_limit {
            Line::OverLimit
        } else if !readable {
            Line::Malformed
        } else {
            Line::Readable
        })
    }

    /// Fills `pending` with the next chunk of the current line.
    fn fill(&mut self) -> io::Result<()> {
        if self.position < self.pending.len() || self.line_ended || self.eof {
            return Ok(());
        }
        self.pending.clear();
        self.position = 0;
        if self.bytes >= MAX_RECORD_BYTES {
            // Past the limit: keep discarding without feeding the parser.
            self.over_limit = true;
            self.start_discard()?;
            return Ok(());
        }
        let available = self.inner.fill_buf()?;
        if available.is_empty() {
            self.eof = true;
            return Ok(());
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(newline) => {
                self.pending.extend_from_slice(&available[..newline]);
                self.inner.consume(newline + 1);
                self.line_ended = true;
            }
            None => {
                let length = available.len();
                self.pending.extend_from_slice(available);
                self.inner.consume(length);
            }
        }
        if !self.pending.is_empty() {
            self.started = true;
        }
        Ok(())
    }

    /// Sets the reader up to throw away the rest of a too-long line.
    fn start_discard(&mut self) -> io::Result<()> {
        self.pending.clear();
        self.position = 0;
        if !self.line_ended && !self.eof {
            loop {
                let available = self.inner.fill_buf()?;
                if available.is_empty() {
                    self.eof = true;
                    break;
                }
                if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                    self.inner.consume(newline + 1);
                    self.line_ended = true;
                    break;
                }
                let length = available.len();
                self.inner.consume(length);
            }
        }
        Ok(())
    }
}

enum Line {
    Readable,
    Blank,
    Malformed,
    OverLimit,
    End,
}

impl<R: BufRead> Read for RecordReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            self.fill()?;
            if self.position < self.pending.len() {
                let length = out.len().min(self.pending.len() - self.position);
                out[..length].copy_from_slice(&self.pending[self.position..self.position + length]);
                self.position += length;
                self.bytes += length as u64;
                self.started = true;
                if self.bytes > MAX_RECORD_BYTES {
                    self.over_limit = true;
                }
                return Ok(length);
            }
            if self.line_ended || self.eof || self.over_limit {
                return Ok(0);
            }
        }
    }
}

/// A harness's `message.content`: a plain string or a list of typed blocks.
#[derive(Clone, Debug, Default)]
pub(crate) struct Content {
    /// The content as one string, when the record wrote it that way.
    pub(crate) text: Option<String>,
    /// Typed blocks in their original order.
    pub(crate) blocks: Vec<Block>,
}

/// One content block.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Block {
    /// Block kind (`text`, `input_text`, `image`, `tool_use`, ...).
    #[serde(rename = "type", default)]
    pub(crate) kind: Option<String>,
    /// Visible text, when the block carries any.
    #[serde(default, deserialize_with = "maybe_text")]
    pub(crate) text: Option<String>,
}

impl Content {
    /// The visible text, trimmed, in the shape both harnesses write.
    pub(crate) fn visible_text(&self) -> String {
        if let Some(text) = &self.text {
            return text.trim().to_owned();
        }
        self.blocks
            .iter()
            .filter(|block| is_text_block(block))
            .filter_map(|block| block.text.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every text block's raw text, exactly as saved.
    pub(crate) fn raw_texts(&self) -> Vec<RawText> {
        if let Some(text) = &self.text {
            return vec![RawText {
                kind: "text".to_owned(),
                text: text.clone(),
            }];
        }
        self.blocks
            .iter()
            .filter_map(|block| {
                block.text.as_ref().map(|text| RawText {
                    kind: block.kind.clone().unwrap_or_default(),
                    text: text.clone(),
                })
            })
            .collect()
    }

    /// Reads `message.content`, accepting a string or a list of blocks.
    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Self>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ContentVisitor;

        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = Option<Content>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or a list of content blocks")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Some(Content {
                    text: Some(value.to_owned()),
                    blocks: Vec::new(),
                }))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(Some(Content {
                    text: Some(value),
                    blocks: Vec::new(),
                }))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut blocks = Vec::new();
                while let Some(block) = sequence.next_element::<Block>()? {
                    blocks.push(block);
                }
                Ok(Some(Content { text: None, blocks }))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // A shape this import does not read (a structured wrapper):
                // skip it without losing the rest of the record.
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }

        deserializer.deserialize_any(ContentVisitor)
    }
}

/// One text block exactly as the transcript saved it.
pub(crate) struct RawText {
    /// Block kind.
    pub(crate) kind: String,
    /// Text, untrimmed.
    pub(crate) text: String,
}

fn is_text_block(block: &Block) -> bool {
    matches!(
        block.kind.as_deref(),
        Some("text" | "input_text" | "output_text")
    )
}

/// Reads a string field, ignoring any other JSON value in its place.
///
/// One field changes shape between records (`payload.message` is a prompt in a
/// user event and an object elsewhere); an unexpected shape must not make the
/// whole record malformed, and it must not allocate a large value either.
pub(crate) fn maybe_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct TextVisitor;

    impl<'de> Visitor<'de> for TextVisitor {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a string")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            Ok(Some(value.to_owned()))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
            Ok(Some(value))
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(TextVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[derive(Deserialize)]
    struct Line {
        #[serde(default, deserialize_with = "maybe_text")]
        text: Option<String>,
        #[serde(default, deserialize_with = "Content::deserialize")]
        content: Option<Content>,
    }

    fn read_all(body: &str) -> Vec<Line> {
        let mut reader = RecordReader::new(BufReader::new(body.as_bytes()));
        let mut out = Vec::new();
        loop {
            match reader.next::<Line>().expect("read") {
                Record::Parsed(line) => out.push(line),
                Record::Blank | Record::Malformed | Record::OverLimit => continue,
                Record::End => return out,
            }
        }
    }

    #[test]
    fn an_over_limit_line_is_reported_apart_from_a_malformed_one() {
        let mut body = String::from("{\"text\":\"fine\"}\n");
        body.push_str("{\"text\":\"");
        body.push_str(&"x".repeat(MAX_RECORD_BYTES as usize + 1));
        body.push_str("\"}\n{\"text\":\"after\"}\n");
        let mut reader = RecordReader::new(BufReader::new(body.as_bytes()));
        let mut seen = Vec::new();
        loop {
            match reader.next::<Line>().expect("read") {
                Record::Parsed(line) => seen.push(format!("parsed:{:?}", line.text)),
                Record::Blank => seen.push("blank".into()),
                Record::Malformed => seen.push("malformed".into()),
                Record::OverLimit => seen.push("over-limit".into()),
                Record::End => break,
            }
        }
        assert_eq!(
            seen,
            [
                "parsed:Some(\"fine\")".to_owned(),
                "over-limit".to_owned(),
                "parsed:Some(\"after\")".to_owned()
            ]
        );
    }

    #[test]
    fn records_stream_one_at_a_time_and_bad_lines_do_not_stop_the_file() {
        let body = "{\"text\":\"one\"}\nnot json\n\n{\"text\":\"two\"}\n";
        let lines = read_all(body);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text.as_deref(), Some("one"));
        assert_eq!(lines[1].text.as_deref(), Some("two"));
    }

    #[test]
    fn a_short_prompt_beside_a_large_ignored_field_is_kept() {
        // A block of the shape a real screenshot attachment takes.
        let padding = "A".repeat(3 * 1024 * 1024);
        let body = format!(
            "{{\"content\":[{{\"type\":\"text\",\"text\":\"keep me\"}},\
             {{\"type\":\"image\",\"source\":{{\"data\":\"{padding}\"}}}}]}}\n"
        );
        let lines = read_all(&body);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].content.as_ref().expect("content").visible_text(),
            "keep me"
        );
    }

    #[test]
    fn an_unexpected_shape_for_a_text_field_does_not_lose_the_record() {
        let body = "{\"content\":{\"nested\":true},\"text\":\"still here\"}\n";
        let lines = read_all(body);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text.as_deref(), Some("still here"));
        assert!(lines[0].content.is_none());
    }
}
