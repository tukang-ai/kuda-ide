#[cfg(test)]
mod tests {
    use kuda_ide::security::PathGuard;
    use std::fs;

    #[test]
    fn test_ipc_security_boundary_integration() {
        let root = std::env::temp_dir().join("kuda_ipc_test_root");
        let _ = fs::create_dir_all(&root);

        let valid_file = root.join("app.rs");
        let _ = fs::write(&valid_file, "fn main() {}");

        let result = PathGuard::validate_path_in_scope(&valid_file, &root);
        assert!(result.is_ok());

        let _ = fs::remove_dir_all(&root);
    }
}
