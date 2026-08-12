mod bash;
mod edit;
mod mutation;
mod path;
mod read;
mod truncate;
mod write;

use std::{io, path::PathBuf};

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::ReadTool;
pub use write::WriteTool;

use crate::ToolSet;

/// Creates pi's default coding-tool quartet rooted at one working directory.
///
/// Relative paths resolve from `cwd`; absolute paths remain available. These
/// tools do not provide a security sandbox, so unattended hosts should run rho
/// inside the containment boundary they intend to trust.
pub fn coding_tools(cwd: impl Into<PathBuf>) -> io::Result<ToolSet> {
    let cwd = path::absolute(cwd.into())?;
    Ok(ToolSet {
        tools: vec![
            Box::new(ReadTool::from_absolute(cwd.clone())),
            Box::new(WriteTool::from_absolute(cwd.clone())),
            Box::new(EditTool::from_absolute(cwd.clone())),
            Box::new(BashTool::from_absolute(cwd)),
        ],
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use rho_ai::{CancellationToken, ContentBlock};
    use serde_json::json;

    use super::*;
    use crate::Tool;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "rho-tools-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if self.0.exists() {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    fn text(output: &crate::ToolOutput) -> &str {
        match output.content.first() {
            Some(ContentBlock::Text { text }) => text,
            _ => panic!("expected text output"),
        }
    }

    #[test]
    fn default_quartet_has_stable_order_and_conservative_replay() {
        let temp = TempDir::new();
        let tools = coding_tools(&temp.0).unwrap();
        let specs = tools.specs();
        assert!(
            specs
                .iter()
                .all(|spec| rho_ai::validate_tool_definition(&spec.definition).is_ok())
        );
        assert_eq!(
            specs
                .iter()
                .map(|spec| spec.definition.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "write", "edit", "bash"]
        );
        assert_eq!(specs[0].replay, rho_core::ReplaySafety::Safe);
        assert!(
            specs[1..]
                .iter()
                .all(|spec| spec.replay == rho_core::ReplaySafety::Never)
        );
    }

    #[tokio::test]
    async fn write_creates_parents_and_read_supports_line_ranges() {
        let temp = TempDir::new();
        let write = WriteTool::new(&temp.0).unwrap();
        let output = write
            .execute(
                json!({"path": "nested/file.txt", "content": "one\ntwo\nthree\n"}),
                CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", text(&output));

        let read = ReadTool::new(&temp.0).unwrap();
        let output = read
            .execute(
                json!({"path": "nested/file.txt", "offset": 2, "limit": 1}),
                CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", text(&output));
        assert!(text(&output).starts_with("two\n"));
        assert!(text(&output).contains("use offset=3"));
    }

    #[tokio::test]
    async fn edit_fuzzy_matches_but_preserves_bom_and_crlf() {
        let temp = TempDir::new();
        let path = temp.0.join("unicode.txt");
        tokio::fs::write(&path, "\u{feff}alpha\r\nsay “hello”   \r\nomega\r\n")
            .await
            .unwrap();
        let edit = EditTool::new(&temp.0).unwrap();
        let output = edit
            .execute(
                json!({
                    "path": "unicode.txt",
                    "edits": [{"old_text": "say \"hello\"", "new_text": "said \"hi\""}]
                }),
                CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", text(&output));
        assert_eq!(
            tokio::fs::read_to_string(path).await.unwrap(),
            "\u{feff}alpha\r\nsaid \"hi\"\r\nomega\r\n"
        );
        assert_eq!(output.details.as_ref().unwrap()["fuzzy_replacements"], 1);
    }

    #[tokio::test]
    async fn concurrent_edits_to_one_file_do_not_lose_updates() {
        let temp = TempDir::new();
        let path = temp.0.join("shared.txt");
        tokio::fs::write(&path, "alpha beta\n").await.unwrap();
        let first = EditTool::new(&temp.0).unwrap();
        let second = EditTool::new(&temp.0).unwrap();
        let (first, second) = tokio::join!(
            first.execute(
                json!({"path": "shared.txt", "edits": [{"old_text": "alpha", "new_text": "A"}]}),
                CancellationToken::new(),
            ),
            second.execute(
                json!({"path": "shared.txt", "edits": [{"old_text": "beta", "new_text": "B"}]}),
                CancellationToken::new(),
            )
        );
        assert!(!first.is_error, "{}", text(&first));
        assert!(!second.is_error, "{}", text(&second));
        assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "A B\n");
    }

    #[tokio::test]
    async fn edit_rejects_duplicate_and_overlapping_matches() {
        let temp = TempDir::new();
        let path = temp.0.join("ambiguous.txt");
        tokio::fs::write(&path, "same same\n").await.unwrap();
        let edit = EditTool::new(&temp.0).unwrap();
        let duplicate = edit
            .execute(
                json!({"path": "ambiguous.txt", "edits": [{"old_text": "same", "new_text": "new"}]}),
                CancellationToken::new(),
            )
            .await;
        assert!(duplicate.is_error);
        assert!(text(&duplicate).contains("2 normalized matches"));

        tokio::fs::write(&path, "say \"hi\"\nsay “hi”\n")
            .await
            .unwrap();
        let normalized_duplicate = edit
            .execute(
                json!({"path": "ambiguous.txt", "edits": [{"old_text": "say \"hi\"", "new_text": "said hi"}]}),
                CancellationToken::new(),
            )
            .await;
        assert!(normalized_duplicate.is_error);
        assert!(text(&normalized_duplicate).contains("2 normalized matches"));

        tokio::fs::write(&path, "abcdef\n").await.unwrap();
        let overlap = edit
            .execute(
                json!({
                    "path": "ambiguous.txt",
                    "edits": [
                        {"old_text": "abcd", "new_text": "one"},
                        {"old_text": "cdef", "new_text": "two"}
                    ]
                }),
                CancellationToken::new(),
            )
            .await;
        assert!(overlap.is_error);
        assert!(text(&overlap).contains("overlaps"));
    }

    #[tokio::test]
    async fn bash_uses_the_configured_cwd_and_observes_cancellation() {
        let temp = TempDir::new();
        let bash = BashTool::new(&temp.0).unwrap();
        let output = bash
            .execute(
                json!({"command": "pwd; printf rho"}),
                CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", text(&output));
        assert!(text(&output).contains(temp.0.to_str().unwrap()));
        assert!(text(&output).contains("rho"));

        let output = bash
            .execute(
                json!({"command": "printf '%060000d' 0"}),
                CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", text(&output));
        assert_eq!(output.details.as_ref().unwrap()["stdout_bytes"], 60_000);
        assert!(text(&output).contains("truncated"));

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let output = bash
            .execute(json!({"command": "printf no"}), cancellation)
            .await;
        assert!(output.is_error);
        assert!(text(&output).contains("cancelled"));

        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            signal.cancel();
        });
        let output = bash
            .execute(json!({"command": "sleep 5"}), cancellation)
            .await;
        assert!(output.is_error);
        assert!(text(&output).contains("cancelled"));
    }

    #[tokio::test]
    async fn read_passes_supported_images_as_binary_content() {
        let temp = TempDir::new();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        tokio::fs::write(temp.0.join("image.png"), bytes)
            .await
            .unwrap();
        let read = ReadTool::new(&temp.0).unwrap();
        let output = read
            .execute(json!({"path": "image.png"}), CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", text(&output));
        assert!(matches!(
            output.content.get(1),
            Some(ContentBlock::Image { data, mime }) if data == bytes && mime == "image/png"
        ));
    }
}
