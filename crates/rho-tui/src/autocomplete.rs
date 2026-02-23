use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    pub trailing_space: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestions {
    pub items: Vec<AutocompleteItem>,
    pub prefix: String,
}

pub trait AutocompleteProvider {
    fn suggestions(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> Option<Suggestions>;
    fn force_file_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> Option<Suggestions>;
}

#[derive(Debug, Clone)]
pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommand>,
    base_path: PathBuf,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<SlashCommand>, base_path: PathBuf) -> Self {
        Self {
            commands,
            base_path,
        }
    }

    pub fn default_commands(base_path: PathBuf) -> Self {
        Self::new(
            vec![
                SlashCommand {
                    name: "cancel".to_string(),
                    description: Some("Cancel in-flight request".to_string()),
                },
                SlashCommand {
                    name: "clear".to_string(),
                    description: Some("Clear local transcript".to_string()),
                },
                SlashCommand {
                    name: "quit".to_string(),
                    description: Some("Quit the UI".to_string()),
                },
            ],
            base_path,
        )
    }

    fn cursor_prefix(lines: &[String], cursor_line: usize, cursor_col: usize) -> Option<String> {
        let line = lines.get(cursor_line)?;
        let prefix: String = line.chars().take(cursor_col).collect();
        Some(prefix)
    }

    fn token_before_cursor(prefix: &str) -> String {
        let mut token = String::new();
        for ch in prefix.chars().rev() {
            if ch.is_whitespace() {
                break;
            }
            token.insert(0, ch);
        }
        token
    }

    fn slash_command_suggestions(&self, prefix: &str) -> Option<Suggestions> {
        if !prefix.starts_with('/') || prefix.contains(' ') {
            return None;
        }

        let query = prefix.trim_start_matches('/');
        let mut ranked: Vec<(i32, &SlashCommand)> = self
            .commands
            .iter()
            .filter_map(|command| fuzzy_score(query, command.name.as_str()).map(|score| (score, command)))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0));

        if ranked.is_empty() {
            return None;
        }

        Some(Suggestions {
            items: ranked
                .into_iter()
                .take(20)
                .map(|(_, command)| AutocompleteItem {
                    value: format!("/{}", command.name),
                    label: command.name.clone(),
                    description: command.description.clone(),
                    trailing_space: true,
                })
                .collect(),
            prefix: prefix.to_string(),
        })
    }

    fn path_suggestions(&self, token: &str, forced: bool) -> Option<Suggestions> {
        let at_mode = token.starts_with('@');
        let raw = if at_mode { &token[1..] } else { token };

        let looks_like_path = raw.contains('/')
            || raw.starts_with('.')
            || raw.starts_with('~')
            || (forced && !raw.starts_with('/'));
        if !looks_like_path && !forced {
            return None;
        }

        let (dir_part, needle) = match raw.rsplit_once('/') {
            Some((dir, tail)) => (Some(format!("{dir}/")), tail.to_string()),
            None => (None, raw.to_string()),
        };

        let search_dir = resolve_search_dir(self.base_path.as_path(), dir_part.as_deref())?;
        let read_dir = fs::read_dir(search_dir).ok()?;

        let mut items = Vec::new();
        for entry in read_dir.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy().to_string();
            if !needle.is_empty()
                && !file_name
                    .to_ascii_lowercase()
                    .starts_with(needle.to_ascii_lowercase().as_str())
            {
                continue;
            }

            let is_directory = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
            let mut completed = String::new();
            if at_mode {
                completed.push('@');
            }
            if let Some(dir) = &dir_part {
                completed.push_str(dir);
            }
            completed.push_str(file_name.as_str());
            if is_directory {
                completed.push('/');
            }

            items.push(AutocompleteItem {
                label: if is_directory {
                    format!("{file_name}/")
                } else {
                    file_name
                },
                description: None,
                value: completed,
                trailing_space: !is_directory,
            });
        }

        if items.is_empty() {
            return None;
        }

        items.sort_by(|a, b| match (a.value.ends_with('/'), b.value.ends_with('/')) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => a.label.cmp(&b.label),
        });

        Some(Suggestions {
            items,
            prefix: token.to_string(),
        })
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn suggestions(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> Option<Suggestions> {
        let before_cursor = Self::cursor_prefix(lines, cursor_line, cursor_col)?;
        if let Some(commands) = self.slash_command_suggestions(before_cursor.as_str()) {
            return Some(commands);
        }

        let token = Self::token_before_cursor(before_cursor.as_str());
        self.path_suggestions(token.as_str(), false)
    }

    fn force_file_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> Option<Suggestions> {
        let before_cursor = Self::cursor_prefix(lines, cursor_line, cursor_col)?;
        if before_cursor.trim_start().starts_with('/') && !before_cursor.contains(' ') {
            return None;
        }

        let token = Self::token_before_cursor(before_cursor.as_str());
        self.path_suggestions(token.as_str(), true)
    }
}

fn resolve_search_dir(base: &Path, dir_part: Option<&str>) -> Option<PathBuf> {
    match dir_part {
        None => Some(base.to_path_buf()),
        Some(dir) if dir.starts_with('/') => Some(PathBuf::from(dir)),
        Some(dir) if dir.starts_with("~/") => {
            let home = std::env::var_os("HOME")?;
            Some(PathBuf::from(home).join(dir.trim_start_matches("~/")))
        }
        Some(dir) => Some(base.join(dir)),
    }
}

fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(1);
    }

    let query = query.to_ascii_lowercase();
    let text = text.to_ascii_lowercase();

    let mut score = 0i32;
    let mut query_index = 0usize;
    let mut last_match = None;
    let query_chars: Vec<char> = query.chars().collect();

    for (index, ch) in text.chars().enumerate() {
        if query_index >= query_chars.len() {
            break;
        }
        if ch == query_chars[query_index] {
            score += 10;
            if let Some(last) = last_match {
                if index == last + 1 {
                    score += 5;
                }
            }
            if index == 0 {
                score += 4;
            }
            last_match = Some(index);
            query_index += 1;
        } else {
            score -= 1;
        }
    }

    if query_index == query_chars.len() {
        Some(score.max(1))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_completion_matches_fuzzy_prefix() {
        let provider = CombinedAutocompleteProvider::default_commands(PathBuf::from("."));
        let suggestions = provider
            .suggestions(&[String::from("/cl")], 0, 3)
            .expect("slash suggestions");

        assert!(suggestions.items.iter().any(|item| item.value == "/clear"));
    }

    #[test]
    fn forced_file_completion_returns_results_from_current_dir() {
        let provider = CombinedAutocompleteProvider::default_commands(PathBuf::from("."));
        let suggestions = provider.force_file_suggestions(&[String::new()], 0, 0);
        assert!(suggestions.is_some());
    }
}
