use tauri::State;
use crate::error::Result;
use crate::state::AppState;
use crate::indexer::ast::{AstParser, CodeSymbol};
use crate::indexer::search::{CodeSearcher, SearchMatch, SearchQuery};
use crate::commands::resolve_path;

#[tauri::command]
pub fn search_code(state: State<'_, AppState>, query: SearchQuery) -> Result<Vec<SearchMatch>> {
    let root = state.require_project_root()?;
    CodeSearcher::search(&query, &root)
}

#[tauri::command]
pub fn search_replace(
    state: State<'_, AppState>,
    query: SearchQuery,
    replacement: String,
    files: Option<Vec<String>>,
) -> Result<crate::indexer::search::SearchReplaceResult> {
    let root = state.require_project_root()?;
    let app_data = state.require_app_data_dir()?;
    CodeSearcher::replace(&query, &replacement, &root, &app_data, files.as_deref())
}

#[tauri::command]
pub fn parse_symbols(state: State<'_, AppState>, path: String) -> Result<Vec<CodeSymbol>> {
    let root = state.require_project_root()?;
    let target = resolve_path(&root, &path);
    let payload = crate::file_system::io::FileSystemIO::read_file(&target, &root, None, None)?;
    AstParser::parse_symbols(&target, &payload.content, &root)
}
