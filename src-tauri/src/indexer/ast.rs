use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tree_sitter::Parser;
use crate::error::{AppError, Result};
use crate::security::PathGuard;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Interface,
    Enum,
    Variable,
    Module,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CodeSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
}

pub struct AstParser;

impl AstParser {
    /// Extracts AST symbols (functions, structs, methods) from source code using Tree-Sitter
    pub fn parse_symbols(file_path: &Path, content: &str, project_root: &Path) -> Result<Vec<CodeSymbol>> {
        let canonical_path = PathGuard::validate_path_in_scope(file_path, project_root)?;
        let ext = canonical_path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let mut parser = Parser::new();
        match ext {
            "rs" => {
                parser
                    .set_language(&tree_sitter_rust::LANGUAGE.into())
                    .map_err(|e| AppError::General(format!("Failed to set Rust grammar: {}", e)))?;
            }
            "py" => {
                parser
                    .set_language(&tree_sitter_python::LANGUAGE.into())
                    .map_err(|e| AppError::General(format!("Failed to set Python grammar: {}", e)))?;
            }
            "ts" | "tsx" => {
                parser
                    .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
                    .map_err(|e| AppError::General(format!("Failed to set TypeScript grammar: {}", e)))?;
            }
            "js" | "jsx" => {
                parser
                    .set_language(&tree_sitter_javascript::LANGUAGE.into())
                    .map_err(|e| AppError::General(format!("Failed to set JavaScript grammar: {}", e)))?;
            }
            _ => return Ok(Vec::new()), // Unsupported extension returns empty symbols
        };

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| AppError::General("Tree-sitter failed to parse tree".to_string()))?;

        // Walk the ENTIRE tree (not just top-level children) so methods, class
        // members, nested functions, enums, interfaces, variables and modules
        // are captured too — the old one-level walk only ever produced
        // Function/Struct symbols.
        let mut symbols = Vec::new();
        let root_node = tree.root_node();
        let mut cursor = root_node.walk();
        let mut stack: Vec<tree_sitter::Node> = root_node.children(&mut cursor).collect();
        while let Some(node) = stack.pop() {
            if let Some(kind) = classify_symbol_kind(node.kind()) {
                let start_line = node.start_position().row + 1;
                let end_line = node.end_position().row + 1;
                if let Ok(snippet) = node.utf8_text(content.as_bytes()) {
                    let first_line = snippet.lines().next().unwrap_or(snippet).to_string();
                    symbols.push(CodeSymbol {
                        name: first_line.clone(),
                        kind,
                        file_path: canonical_path.clone(),
                        start_line,
                        end_line,
                        signature: Some(first_line),
                    });
                }
            }
            let mut child_cursor = node.walk();
            for child in node.children(&mut child_cursor) {
                stack.push(child);
            }
        }

        Ok(symbols)
    }
}

/// Maps tree-sitter node kinds across the Rust / Python / TS / JS grammars to
/// the codebase's `SymbolKind`. Unknown kinds are skipped.
fn classify_symbol_kind(kind: &str) -> Option<SymbolKind> {
    match kind {
        // Functions / methods (Rust, Python, TS/JS).
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "method_definition"
        | "method_declaration"
        | "arrow_function"
        | "function_expression"
        | "generator_function"
        | "generator_function_declaration" => Some(SymbolKind::Function),
        "struct_item" | "struct_declaration" => Some(SymbolKind::Struct),
        "class_definition" | "class_declaration" => Some(SymbolKind::Class),
        "interface_declaration" => Some(SymbolKind::Interface),
        "enum_item" | "enum_declaration" => Some(SymbolKind::Enum),
        "lexical_declaration" | "variable_declaration" => Some(SymbolKind::Variable),
        "mod_item" | "module_declaration" => Some(SymbolKind::Module),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_sitter_rust_parse() {
        let temp_dir = std::env::temp_dir().join("kuda_ast_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let f1 = temp_dir.join("main.rs");
        let code = "fn hello_world() {\n    println!(\"hi\");\n}\n";

        let symbols = AstParser::parse_symbols(&f1, code, &temp_dir).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].kind, SymbolKind::Function);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
