use std::fs;
use std::path::Path;

fn clean_apple_double<P: AsRef<Path>>(dir: P) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if file_name.starts_with("._") {
                    let _ = fs::remove_file(&path);
                } else if path.is_dir() {
                    clean_apple_double(&path);
                }
            }
        }
    }
}

fn main() {
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        clean_apple_double(&out_dir);
    }
    if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
        clean_apple_double(&target_dir);
    }
    tauri_build::build();
}
