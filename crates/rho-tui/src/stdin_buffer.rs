#![allow(dead_code)]

const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
const BRACKETED_PASTE_END: &str = "\u{1b}[201~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinBufferChunk {
    Data(String),
    Paste(String),
}

#[derive(Debug, Clone, Default)]
pub struct StdinBuffer {
    input_buffer: String,
    paste_mode: bool,
    paste_buffer: String,
}

impl StdinBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn process(&mut self, chunk: &str) -> Vec<StdinBufferChunk> {
        self.input_buffer.push_str(chunk);
        let mut output = Vec::new();

        loop {
            if self.paste_mode {
                if let Some(end_index) = self.input_buffer.find(BRACKETED_PASTE_END) {
                    self.paste_buffer.push_str(&self.input_buffer[..end_index]);
                    self.input_buffer
                        .replace_range(..end_index + BRACKETED_PASTE_END.len(), "");
                    output.push(StdinBufferChunk::Paste(std::mem::take(
                        &mut self.paste_buffer,
                    )));
                    self.paste_mode = false;
                    continue;
                }

                self.paste_buffer.push_str(&self.input_buffer);
                self.input_buffer.clear();
                break;
            }

            let Some(start_index) = self.input_buffer.find(BRACKETED_PASTE_START) else {
                if !self.input_buffer.is_empty() {
                    output.push(StdinBufferChunk::Data(std::mem::take(
                        &mut self.input_buffer,
                    )));
                }
                break;
            };

            if start_index > 0 {
                output.push(StdinBufferChunk::Data(
                    self.input_buffer[..start_index].to_string(),
                ));
            }
            self.input_buffer
                .replace_range(..start_index + BRACKETED_PASTE_START.len(), "");
            self.paste_mode = true;
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_data_for_plain_input() {
        let mut buffer = StdinBuffer::new();
        let chunks = buffer.process("hello");
        assert_eq!(chunks, vec![StdinBufferChunk::Data("hello".to_string())]);
    }

    #[test]
    fn emits_paste_chunk_for_bracketed_input() {
        let mut buffer = StdinBuffer::new();
        let chunks = buffer.process("\u{1b}[200~line1\nline2\u{1b}[201~");
        assert_eq!(
            chunks,
            vec![StdinBufferChunk::Paste("line1\nline2".to_string())]
        );
    }

    #[test]
    fn handles_paste_split_across_chunks() {
        let mut buffer = StdinBuffer::new();
        let first = buffer.process("\u{1b}[200~line1");
        assert!(first.is_empty());
        let second = buffer.process("\nline2\u{1b}[201~");
        assert_eq!(
            second,
            vec![StdinBufferChunk::Paste("line1\nline2".to_string())]
        );
    }

    #[test]
    fn handles_mixed_text_and_paste() {
        let mut buffer = StdinBuffer::new();
        let chunks = buffer.process("a\u{1b}[200~b\u{1b}[201~c");
        assert_eq!(
            chunks,
            vec![
                StdinBufferChunk::Data("a".to_string()),
                StdinBufferChunk::Paste("b".to_string()),
                StdinBufferChunk::Data("c".to_string())
            ]
        );
    }
}
