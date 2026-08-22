use grep::searcher::sinks::UTF8;
use grep::searcher::Searcher;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use crate::error::{AppError, Result};
use crate::security::PathGuard;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SearchQuery {
    pub pattern: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    pub max_results: Option<usize>,
    /// When true (the default for literal search mode), the replacement string
    /// is inserted VERBATIM. When false (regex mode), `$1`/`${name}` group
    /// references in the replacement are expanded, matching VS Code semantics.
    /// This flag exists because `regex::replace_all` treats `$` sequences as
    /// template syntax by default — replacing `cost` with `$100` used to
    /// silently DELETE every match across the whole workspace.
    #[serde(default = "default_true")]
    pub replacement_is_literal: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SearchMatch {
    pub file_path: PathBuf,
    pub line_number: u64,
    pub line_content: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SearchReplaceResult {
    pub files_changed: usize,
    pub matches_replaced: usize,
    pub files: Vec<PathBuf>,
}

pub struct CodeSearcher;

impl CodeSearcher {
    /// Executes fast ripgrep content search across the project root
    pub fn search(query: &SearchQuery, project_root: &Path) -> Result<Vec<SearchMatch>> {
        let canonical_root = PathGuard::validate_path_in_scope(project_root, project_root)?;
        let max_results = query.max_results.unwrap_or(500);

        // Honor the case-sensitivity flag: the matcher used to be built with the
        // crate default (case-sensitive), silently ignoring `query.case_sensitive`.
        let matcher = grep::regex::RegexMatcherBuilder::new()
            .case_insensitive(!query.case_sensitive)
            .build(&query.pattern)
            .map_err(|e| AppError::General(format!("Invalid search regex pattern: {}", e)))?;

        let mut matches = Vec::new();
        let walker = WalkBuilder::new(&canonical_root)
            // Skip hidden files/dirs: `.env`, `.env.local`, credential dotfiles
            // must never be greppable into the agent context (inconsistent with
            // code_outline's `hidden(true)`; a hidden `.env` not covered by
            // .gitignore used to leak secrets into the model context).
            .hidden(true)
            .git_ignore(true)
            .build();

        for result in walker {
            if matches.len() >= max_results {
                break;
            }

            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let file_path = entry.path().to_path_buf();
            let file_path_clone = file_path.clone();

            let mut searcher = Searcher::new();
            let _ = searcher.search_path(
                &matcher,
                &file_path,
                UTF8(|line_num, line| {
                    matches.push(SearchMatch {
                        file_path: file_path_clone.clone(),
                        line_number: line_num,
                        line_content: line.trim_end().to_string(),
                    });
                    Ok(matches.len() < max_results)
                }),
            );
        }

        Ok(matches)
    }

    /// Replaces every occurrence of `query.pattern` with `replacement` across
    /// the project (respecting `case_sensitive`), writing each changed file
    /// with an automatic checkpoint (session-tagged) through the normal atomic
    /// write path. This powers the Search panel's "Replace All".
    ///
    /// `files_filter` restricts the operation to an explicit file set — the
    /// panel passes the paths visible in its FILTERED result list, so scope /
    /// include / exclude filters are honored instead of silently rewriting
    /// every matching file in the workspace.
    pub fn replace(
        query: &SearchQuery,
        replacement: &str,
        project_root: &Path,
        app_data_dir: &Path,
        files_filter: Option<&[String]>,
    ) -> Result<SearchReplaceResult> {
        let canonical_root = PathGuard::validate_path_in_scope(project_root, project_root)?;
        let re = regex::RegexBuilder::new(&query.pattern)
            .case_insensitive(!query.case_sensitive)
            .build()
            .map_err(|e| AppError::General(format!("Invalid search regex pattern: {}", e)))?;

        let checkpoint_mgr = crate::diff_engine::history::CheckpointManager::new(app_data_dir)?;
        let mut files_changed = 0usize;
        let mut matches_replaced = 0usize;
        let mut files: Vec<PathBuf> = Vec::new();

        let walker = WalkBuilder::new(&canonical_root)
            .hidden(true)
            .git_ignore(true)
            .build();
        for result in walker {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }
            let file_path = entry.path().to_path_buf();
            if let Some(allowed) = files_filter {
                let fp_str = file_path.to_string_lossy();
                if !allowed.iter().any(|a| a.as_str() == fp_str.as_ref()) {
                    continue;
                }
            }
            let content = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => continue, // binary/unreadable files are skipped
            };
            if !re.is_match(&content) {
                continue;
            }
            // Literal mode inserts the replacement verbatim; `$100` stays
            // `$100`. Only explicit regex mode expands group references.
            let new_content = if query.replacement_is_literal {
                re.replace_all(&content, |_: &regex::Captures| replacement.to_string())
                    .to_string()
            } else {
                re.replace_all(&content, replacement).to_string()
            };
            if new_content == content {
                continue;
            }
            let matches_in_file = re.find_iter(&content).count();
            crate::file_system::io::FileSystemIO::write_file_canonical_in_session(
                &file_path,
                &new_content,
                &checkpoint_mgr,
                None,
                None,
            )?;
            files_changed += 1;
            matches_replaced += matches_in_file;
            files.push(file_path);
        }

        Ok(SearchReplaceResult {
            files_changed,
            matches_replaced,
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_ripgrep_search() {
        let temp_dir = std::env::temp_dir().join("kuda_search_test");
        let _ = fs::create_dir_all(&temp_dir);

        let f1 = temp_dir.join("main.rs");
        let _ = fs::write(&f1, "fn main() {\n    let target_var = 42;\n}\n");

        let query = SearchQuery {
            pattern: "target_var".to_string(),
            is_regex: false,
            case_sensitive: true,
            max_results: Some(10),
            replacement_is_literal: true,
        };

        let results = CodeSearcher::search(&query, &temp_dir).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line_number, 2);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
