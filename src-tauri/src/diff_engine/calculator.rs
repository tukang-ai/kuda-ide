use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ChangeKind {
    Insert,
    Delete,
    Equal,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiffChange {
    pub kind: ChangeKind,
    pub content: String,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiffResult {
    pub file_path: PathBuf,
    pub original_content: String,
    pub modified_content: String,
    pub changes: Vec<DiffChange>,
    pub insertions: usize,
    pub deletions: usize,
}

pub struct DiffCalculator;

impl DiffCalculator {
    /// Computes line-by-line surgical diff between original_content and modified_content
    pub fn compute_diff(file_path: PathBuf, original_content: &str, modified_content: &str) -> DiffResult {
        let diff = TextDiff::from_lines(original_content, modified_content);
        let mut changes = Vec::new();
        let mut insertions = 0;
        let mut deletions = 0;

        for change in diff.iter_all_changes() {
            let kind = match change.tag() {
                ChangeTag::Insert => {
                    insertions += 1;
                    ChangeKind::Insert
                }
                ChangeTag::Delete => {
                    deletions += 1;
                    ChangeKind::Delete
                }
                ChangeTag::Equal => ChangeKind::Equal,
            };

            changes.push(DiffChange {
                kind,
                content: change.value().to_string(),
                old_line: change.old_index(),
                new_line: change.new_index(),
            });
        }

        DiffResult {
            file_path,
            original_content: original_content.to_string(),
            modified_content: modified_content.to_string(),
            changes,
            insertions,
            deletions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_diff() {
        let old_text = "line 1\nline 2\n";
        let new_text = "line 1\nline 2 modified\nline 3\n";

        let diff = DiffCalculator::compute_diff(PathBuf::from("test.rs"), old_text, new_text);
        assert!(diff.insertions > 0);
        assert!(diff.deletions > 0);
    }
}
