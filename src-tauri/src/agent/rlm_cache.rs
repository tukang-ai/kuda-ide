//! Persistent RLM research cache, stored OUTSIDE the project tree in
//! `<app_data_dir>/rlm_cache/`, keyed per project (by a stable hash of the
//! project root path). Contains:
//! - `manifest.json`: a snapshot of the project tree (size/mtime/sha256 per
//!   file, never the content itself) used to detect what changed between chats.
//! - `brief.json` / `audit.json` / `brief_digest.txt`: the last validated RLM
//!   research so a new chat can reuse it (sufficiency flow) instead of
//!   re-researching from zero.
//! - `inventory.json`: files already loaded into the persistent Python kernel
//!   so a fresh kernel can be pre-warmed without re-reading.
//!
//! All loads fail open: a corrupt/legacy cache resolves to `None` and the
//! swarm falls back to a full fresh research run.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::swarm::{ContextAudit, ResearchBrief};
use crate::error::Result;

pub const SCHEMA_VERSION: u32 = 1;

/// Max files the manifest tracks (bounds cost on huge repos).
pub const MAX_MANIFEST_FILES: usize = 8000;
/// Files larger than this are skipped by the manifest (not indexed).
pub const MAX_MANIFEST_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Directories the manifest skips (dependency/build output trees).
pub fn is_heavy_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | "out"
            | "vendor"
            | ".venv"
            | "__pycache__"
            | ".cache"
            | "coverage"
            | "Pods"
            | ".pytest_cache"
            | ".kuda"
    )
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileManifestEntry {
    pub size: u64,
    pub mtime_ns: i64,
    /// Change time (nanoseconds since epoch) on unix, falling back to mtime on
    /// platforms without ctime. ANY write bumps ctime even when mtime is
    /// deliberately restored (`cp -p`, `rsync --times`, `touch -r`), so the
    /// fast path stays sound. `0` means "unknown" (legacy manifests) and forces
    /// a re-hash instead of trusting a possibly-stale sha.
    #[serde(default)]
    pub ctime_ns: i64,
    pub sha256: String,
}

/// ctime in nanoseconds since epoch (unix), or mtime as a fallback where ctime
/// is unavailable (keeps the crate compiling on Windows).
fn metadata_ctime_ns(meta: &fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.ctime() as i64)
            .checked_mul(1_000_000_000)
            .and_then(|secs| secs.checked_add(meta.ctime_nsec() as i64))
            .unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        meta.modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileManifest {
    pub generated_at: DateTime<Local>,
    pub file_count: usize,
    pub files: HashMap<String, FileManifestEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectCacheMeta {
    pub schema_version: u32,
    pub project_root: String,
    pub created_at: DateTime<Local>,
    pub last_used: DateTime<Local>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KernelInventory {
    /// Relative paths (to the project root) already loaded into the kernel.
    pub loaded_paths: Vec<String>,
    pub generated_at: DateTime<Local>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IndexEntry {
    pub path: String,
    pub hash: String,
    pub last_used: DateTime<Local>,
    pub last_research_at: DateTime<Local>,
    pub brief_summary: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RlmCacheIndex {
    pub projects: Vec<IndexEntry>,
}

/// All cached artifacts for one project. Every optional field fails open.
pub struct ProjectCache {
    pub key: String,
    pub meta: ProjectCacheMeta,
    pub manifest: Option<FileManifest>,
    pub brief: Option<ResearchBrief>,
    pub audit: Option<ContextAudit>,
    pub digest: Option<String>,
    pub inventory: Option<KernelInventory>,
}

pub struct RlmCacheStore {
    root: PathBuf,
}

/// Diff between two manifests, used to classify how a swarm run should treat
/// the cached brief.
#[derive(Debug, Clone, Default)]
pub struct ManifestDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
    pub unchanged_count: usize,
    pub changed_ratio: f32,
}

impl ManifestDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    /// All relative paths that were added/removed/modified.
    pub fn all_changed(&self) -> Vec<String> {
        self.added
            .iter()
            .chain(self.removed.iter())
            .chain(self.modified.iter())
            .cloned()
            .collect()
    }
}

/// How a new swarm run should treat the cached research brief.
#[derive(Debug, Clone, PartialEq)]
pub enum CacheDecision {
    /// Cache valid and project unchanged → the RLM Model only checks whether
    /// the brief covers this request (cheap turns), no open-ended exploration.
    Sufficiency,
    /// Cache valid as a reference but some files changed → collect/refresh
    /// only the affected pieces, starting from the cached brief.
    Incremental,
    /// No cache / massive change / stale → full fresh research (old brief may
    /// still be offered as an explicitly-labeled reference).
    Fresh,
}

impl RlmCacheStore {
    pub fn new(app_data_dir: &Path) -> Self {
        Self {
            root: app_data_dir.join("rlm_cache"),
        }
    }

    /// Stable per-project key: first 16 hex chars of sha256(canonical root).
    pub fn project_key(project_root: &Path) -> String {
        let canon = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canon.to_string_lossy().as_bytes());
        let hex = format!("{:x}", hasher.finalize());
        hex.chars().take(16).collect()
    }

    fn project_dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Loads the full cache for a project, or `None` on any error/corruption.
    pub fn load(&self, project_root: &Path) -> Option<ProjectCache> {
        let key = Self::project_key(project_root);
        let dir = self.project_dir(&key);
        if !dir.is_dir() {
            return None;
        }
        let read = |name: &str| fs::read_to_string(dir.join(name)).ok();
        let meta: ProjectCacheMeta = match read("meta.json") {
            Some(s) => match serde_json::from_str(&s) {
                Ok(m) => m,
                Err(_) => return None,
            },
            None => return None,
        };
        if meta.schema_version != SCHEMA_VERSION {
            return None;
        }
        Some(ProjectCache {
            key,
            meta,
            manifest: read("manifest.json").and_then(|s| serde_json::from_str(&s).ok()),
            brief: read("brief.json").and_then(|s| serde_json::from_str(&s).ok()),
            audit: read("audit.json").and_then(|s| serde_json::from_str(&s).ok()),
            digest: read("brief_digest.txt"),
            inventory: read("inventory.json").and_then(|s| serde_json::from_str(&s).ok()),
        })
    }

    /// Persists the full cache for a project after a successful validated
    /// research run. All writes are atomic (tmp + rename) so a crash never
    /// leaves a half-written cache.
    pub fn save(
        &self,
        project_root: &Path,
        brief: &ResearchBrief,
        audit: &ContextAudit,
        digest: &str,
        manifest: &FileManifest,
        inventory: &KernelInventory,
    ) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let key = Self::project_key(project_root);
        let dir = self.project_dir(&key);
        fs::create_dir_all(&dir)?;
        let now = Local::now();
        let meta = ProjectCacheMeta {
            schema_version: SCHEMA_VERSION,
            project_root: project_root.to_string_lossy().to_string(),
            created_at: now,
            last_used: now,
        };
        self.write_atomic(&dir, "meta.json", &serde_json::to_string_pretty(&meta)?)?;
        self.write_atomic(&dir, "manifest.json", &serde_json::to_string_pretty(manifest)?)?;
        self.write_atomic(&dir, "brief.json", &serde_json::to_string_pretty(brief)?)?;
        self.write_atomic(&dir, "audit.json", &serde_json::to_string_pretty(audit)?)?;
        self.write_atomic(&dir, "brief_digest.txt", digest)?;
        self.write_atomic(&dir, "inventory.json", &serde_json::to_string_pretty(inventory)?)?;
        self.update_index(project_root, brief)?;
        Ok(())
    }

    /// Updates the global registry (`index.json`) used for cross-project lookup.
    pub fn update_index(&self, project_root: &Path, brief: &ResearchBrief) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let key = Self::project_key(project_root);
        let now = Local::now();
        let mut index = self.load_index();
        if let Some(entry) = index.projects.iter_mut().find(|e| e.hash == key) {
            entry.path = project_root.to_string_lossy().to_string();
            entry.last_used = now;
            entry.last_research_at = now;
            entry.brief_summary = brief.summary.clone();
        } else {
            index.projects.push(IndexEntry {
                path: project_root.to_string_lossy().to_string(),
                hash: key,
                last_used: now,
                last_research_at: now,
                brief_summary: brief.summary.clone(),
            });
        }
        self.write_atomic(&self.root, "index.json", &serde_json::to_string_pretty(&index)?)?;
        Ok(())
    }

    /// Returns a project's `brief_digest.txt` snapshot (for cross-project reads).
    pub fn load_digest_for_project(&self, project_root: &Path) -> Option<String> {
        let key = Self::project_key(project_root);
        fs::read_to_string(self.project_dir(&key).join("brief_digest.txt")).ok()
    }

    pub fn load_index(&self) -> RlmCacheIndex {
        fs::read_to_string(self.root.join("index.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_atomic(&self, dir: &Path, name: &str, contents: &str) -> Result<()> {
        let tmp = dir.join(format!(".{}.tmp", name));
        // Mode 0600: the cache stores VERBATIM code snippets / briefs which may
        // contain sensitive project data — it must not be world-readable.
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, dir.join(name))?;
        Ok(())
    }
}

/// Walks the project tree (respecting `.gitignore`) and builds a manifest.
/// Reuses the stored sha256 for files whose `(size, mtime_ns)` did not change
/// (fast path), hashing only files that actually drifted.
pub fn build_manifest(
    project_root: &Path,
    old: Option<&FileManifest>,
) -> Result<FileManifest> {
    let mut files: HashMap<String, FileManifestEntry> = HashMap::new();
    let walker = ignore::WalkBuilder::new(project_root)
        // NOTE: hidden files stay included on purpose — the manifest stores only
        // size/mtime/sha256 METADATA (never content), and tracking `.gitignore`
        // etc. is required for correct cache invalidation.
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|e| {
            e.file_type()
                .map_or(true, |ft| !ft.is_dir() || !is_heavy_dir(e.file_name().to_string_lossy().as_ref()))
        })
        .build();

    for entry in walker.flatten() {
        if files.len() >= MAX_MANIFEST_FILES {
            break;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = path.metadata() else {
            continue;
        };
        if meta.len() > MAX_MANIFEST_FILE_BYTES {
            continue;
        }
        let rel = path.strip_prefix(project_root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().to_string();
        if rel_str.is_empty() {
            continue;
        }
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let ctime_ns = metadata_ctime_ns(&meta);

        // Fast path: reuse the stored sha when (size, mtime, ctime) all match.
        // ctime catches content writes that deliberately restore mtime; a
        // legacy entry (ctime_ns == 0, or an unknown ctime from the OS) must
        // NOT take this fast path — it is re-hashed to stay sound against
        // timestamp-preserving edits (`cp -p`, `rsync --times`, `touch -r`).
        let sha256 = match old.and_then(|o| o.files.get(&rel_str)) {
            Some(e)
                if e.size == meta.len()
                    && e.mtime_ns == mtime_ns
                    && e.ctime_ns != 0
                    && e.ctime_ns == ctime_ns =>
            {
                e.sha256.clone()
            }
            _ => hash_file(path)?,
        };
        files.insert(
            rel_str,
            FileManifestEntry {
                size: meta.len(),
                mtime_ns,
                ctime_ns,
                sha256,
            },
        );
    }

    Ok(FileManifest {
        generated_at: Local::now(),
        file_count: files.len(),
        files,
    })
}

/// Hashes a file's raw bytes with SHA256.
pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Diffs `new` against the `old` manifest, classifying each path and computing
/// the fraction of the tree that changed.
pub fn diff_manifest(old: &FileManifest, new: &FileManifest) -> ManifestDiff {
    let mut diff = ManifestDiff::default();
    let mut total = 0usize;
    for (rel, entry) in &new.files {
        total += 1;
        match old.files.get(rel) {
            Some(oe) if oe.size == entry.size && oe.mtime_ns == entry.mtime_ns && oe.sha256 == entry.sha256 => {
                diff.unchanged_count += 1;
            }
            Some(_) => diff.modified.push(rel.clone()),
            None => diff.added.push(rel.clone()),
        }
    }
    for rel in old.files.keys() {
        if !new.files.contains_key(rel) {
            diff.removed.push(rel.clone());
        }
    }
    let changed = diff.added.len() + diff.removed.len() + diff.modified.len();
    diff.changed_ratio = if total > 0 {
        changed as f32 / total as f32
    } else {
        0.0
    };
    diff
}

/// Normalizes a path written in a brief (relative or absolute) to a
/// manifest-relative path so anchor checks match regardless of how the model
/// wrote it. Handles symlink-inconsistent roots (`/var` vs `/private/var` on
/// macOS) and already-deleted files.
fn normalize_relative(path: &str, project_root: &Path) -> String {
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        return path.trim_start_matches('/').to_string();
    }
    let canon_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    // 1. Canonicalized match (both sides resolved; works when the file exists).
    if let Ok(abs) = p.canonicalize() {
        if let Ok(rel) = abs.strip_prefix(&canon_root) {
            let s = rel.to_string_lossy().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }

    // 2. String-prefix strip against the canonical AND the raw root, covering
    //    symlinked roots and already-deleted files (canonicalize fails then).
    let path_str = p.to_string_lossy().to_string();
    let mut candidates: Vec<String> = Vec::with_capacity(2);
    candidates.push(canon_root.to_string_lossy().to_string());
    let raw_root = project_root.to_string_lossy().to_string();
    if !candidates.contains(&raw_root) {
        candidates.push(raw_root);
    }
    for candidate in candidates {
        if let Some(stripped) = path_str.strip_prefix(&candidate) {
            let s = stripped
                .trim_start_matches(std::path::MAIN_SEPARATOR)
                .to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }

    // 3. Fallback: root-relative.
    path.trim_start_matches('/').to_string()
}

/// Classifies how a swarm run should treat the cached brief (pure function,
/// unit-testable without touching the LLM).
///
/// - No usable cache → `Fresh`.
/// - The brief's own `key_files` changed or disappeared → `Fresh` (a brief
///   anchored to now-stale files would mislead the Thinker).
/// - Project unchanged → `Sufficiency`.
/// - Small drift → `Incremental`.
/// - Changed ratio above `changed_ratio_threshold` → `Fresh`.
/// - Research older than `max_age` → `Fresh`.
pub fn classify_cache_state(
    project_root: &Path,
    cached: Option<&ProjectCache>,
    diff: Option<&ManifestDiff>,
    changed_ratio_threshold: f32,
    max_age: chrono::Duration,
) -> CacheDecision {
    let Some(cached) = cached else {
        return CacheDecision::Fresh;
    };
    if cached.brief.is_none() || cached.audit.is_none() || cached.manifest.is_none() {
        return CacheDecision::Fresh;
    }
    let Some(diff) = diff else {
        return CacheDecision::Fresh;
    };

    // Age gate: a very old research is treated as fresh even if files are
    // byte-identical (external factors may have changed the world).
    if let Some(manifest) = cached.manifest.as_ref() {
        if Local::now().signed_duration_since(manifest.generated_at) > max_age {
            return CacheDecision::Fresh;
        }
    }

    if diff.changed_ratio > changed_ratio_threshold {
        return CacheDecision::Fresh;
    }

    // The brief's key files are its anchors: if any drifted or vanished, the
    // cached research is no longer trustworthy as-is.
    if let Some(brief) = cached.brief.as_ref() {
        let anchor_moved = brief.key_files.iter().any(|kf| {
            let rel = normalize_relative(&kf.path, project_root);
            diff.modified.iter().any(|m| m == &rel) || diff.removed.iter().any(|r| r == &rel)
        });
        if anchor_moved {
            return CacheDecision::Fresh;
        }
    }

    if diff.is_empty() {
        CacheDecision::Sufficiency
    } else {
        CacheDecision::Incremental
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn test_project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("kuda_rlm_cache_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/out.bin"), b"binary").unwrap();
        root
    }

    #[test]
    fn test_project_key_stable_and_scoped() {
        let r1 = test_project("key1");
        let r2 = test_project("key2");
        let k1 = RlmCacheStore::project_key(&r1);
        assert_eq!(k1, RlmCacheStore::project_key(&r1));
        assert_ne!(k1, RlmCacheStore::project_key(&r2));
        assert_eq!(k1.len(), 16);
    }

    #[test]
    fn test_manifest_ignores_gitignored_and_heavy_dirs() {
        let root = test_project("manifest");
        let m = build_manifest(&root, None).unwrap();
        assert!(m.files.contains_key("src/main.rs"));
        assert!(!m.files.contains_key("target/out.bin"));
    }

    #[test]
    fn test_manifest_diff_detects_change_and_reuses_sha() {
        let root = test_project("diff");
        let m1 = build_manifest(&root, None).unwrap();
        let sha_before = m1.files["src/main.rs"].sha256.clone();

        std::fs::write(root.join("src/main.rs"), "fn main() { println!(); }\n").unwrap();
        let m2 = build_manifest(&root, Some(&m1)).unwrap();
        assert_ne!(sha_before, m2.files["src/main.rs"].sha256);

        // Unchanged files keep their sha without re-hashing (reuse via old).
        let m3 = build_manifest(&root, Some(&m2)).unwrap();
        assert_eq!(m2.files["src/main.rs"].sha256, m3.files["src/main.rs"].sha256);

        let diff = diff_manifest(&m1, &m2);
        assert_eq!(diff.modified, vec!["src/main.rs".to_string()]);
        assert!(diff.modified.len() > 0);
        assert!(!diff.is_empty());
    }

    #[test]
    fn test_save_load_roundtrip_and_corruption_fails_open() {
        let root = test_project("roundtrip");
        let app_dir = root.join("app_data");
        let store = RlmCacheStore::new(&app_dir);

        let manifest = build_manifest(&root, None).unwrap();
        let brief = ResearchBrief {
            summary: "curated summary".into(),
            key_files: vec![],
            ..Default::default()
        };
        let audit = ContextAudit {
            complete: true,
            summary: "ok".into(),
            missing: vec![],
        };
        let inventory = KernelInventory {
            loaded_paths: vec!["src/main.rs".into()],
            generated_at: Local::now(),
        };
        store
            .save(&root, &brief, &audit, "## SUMMARY\ncurated summary", &manifest, &inventory)
            .unwrap();

        let loaded = store.load(&root).expect("cache must load after save");
        assert_eq!(loaded.brief.unwrap().summary, "curated summary");
        assert_eq!(loaded.digest.unwrap(), "## SUMMARY\ncurated summary");
        assert_eq!(loaded.inventory.unwrap().loaded_paths, vec!["src/main.rs".to_string()]);
        let manifest = loaded.manifest.unwrap();
        assert!(manifest.files.contains_key("src/main.rs"));
        assert!(manifest.files.contains_key(".gitignore"));

        // Corrupt the brief: load must fail open (brief None → classify → Fresh).
        std::fs::write(app_dir.join("rlm_cache").join(&loaded.key).join("brief.json"), "{corrupt").unwrap();
        let after_corrupt = store.load(&root);
        assert!(
            after_corrupt.as_ref().map(|c| c.brief.is_none()).unwrap_or(true),
            "corrupt brief must fail open to None, got {:?}",
            after_corrupt.as_ref().map(|c| c.brief.is_some())
        );

        // No .tmp leftovers.
        let dir = app_dir.join("rlm_cache").join(&loaded.key);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "no tmp leftovers: {:?}", leftovers);
    }

    #[test]
    fn test_classify_cache_state_modes() {
        let app_dir = std::env::temp_dir().join("rlm_cache_classify");
        let root = app_dir.join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        // A 10-file tree so a single edit stays well under the 30% threshold.
        for i in 0..10 {
            std::fs::write(root.join(format!("src/file_{}.rs", i)), format!("fn f{}() {{}}\n", i)).unwrap();
        }

        let store = RlmCacheStore::new(&app_dir);
        let manifest = build_manifest(&root, None).unwrap();
        let brief = ResearchBrief {
            summary: "s".into(),
            key_files: vec![],
            ..Default::default()
        };
        let audit = ContextAudit { complete: true, summary: "ok".into(), missing: vec![] };
        let inv = KernelInventory { loaded_paths: vec![], generated_at: Local::now() };
        store
            .save(&root, &brief, &audit, "d", &manifest, &inv)
            .unwrap();

        let cached = store.load(&root).unwrap();
        let new_manifest = build_manifest(&root, Some(&cached.manifest.as_ref().unwrap())).unwrap();
        let empty_diff = diff_manifest(cached.manifest.as_ref().unwrap(), &new_manifest);
        assert_eq!(
            classify_cache_state(&root, Some(&cached), Some(&empty_diff), 0.3, Duration::days(30)),
            CacheDecision::Sufficiency
        );

        // Change one file of ten → Incremental.
        std::fs::write(root.join("src/file_0.rs"), "fn f0() { println!(); }\n").unwrap();
        let new_manifest2 = build_manifest(&root, Some(cached.manifest.as_ref().unwrap())).unwrap();
        let small_diff = diff_manifest(cached.manifest.as_ref().unwrap(), &new_manifest2);
        assert_eq!(
            classify_cache_state(&root, Some(&cached), Some(&small_diff), 0.3, Duration::days(30)),
            CacheDecision::Incremental
        );

        // No cache → Fresh.
        assert_eq!(
            classify_cache_state(&root, None, Some(&empty_diff), 0.3, Duration::days(30)),
            CacheDecision::Fresh
        );

        // Brief key file removed → Fresh.
        let mut anchored_brief = brief.clone();
        anchored_brief.key_files.push(crate::agent::swarm::BriefFile {
            path: "src/file_0.rs".into(),
            why: "anchor".into(),
            key_symbols: vec![],
        });
        let store2 = RlmCacheStore::new(&app_dir.join("other"));
        store2
            .save(&root, &anchored_brief, &audit, "d", &manifest, &inv)
            .unwrap();
        let cached2 = store2.load(&root).unwrap();
        std::fs::remove_file(root.join("src/file_0.rs")).unwrap();
        let removed_manifest = build_manifest(&root, Some(&manifest)).unwrap();
        let removed_diff = diff_manifest(&manifest, &removed_manifest);
        assert_eq!(
            classify_cache_state(&root, Some(&cached2), Some(&removed_diff), 0.3, Duration::days(30)),
            CacheDecision::Fresh
        );

        let _ = std::fs::remove_dir_all(&app_dir);
    }

    #[test]
    fn test_manifest_ctime_changes_but_sha_stable() {
        let root = test_project("ctime");
        let m1 = build_manifest(&root, None).unwrap();
        let ctime_before = m1.files["src/main.rs"].ctime_ns;
        assert_ne!(ctime_before, 0, "ctime must be recorded on unix");

        // Consecutive builds on an unchanged tree reuse sha AND keep ctime.
        let m2 = build_manifest(&root, Some(&m1)).unwrap();
        assert_eq!(m1.files["src/main.rs"].sha256, m2.files["src/main.rs"].sha256);
        assert_eq!(ctime_before, m2.files["src/main.rs"].ctime_ns);

        // A write bumps ctime (and content) → new sha, manifest marks modified.
        std::fs::write(root.join("src/main.rs"), "fn main() { println!(); }\n").unwrap();
        let m3 = build_manifest(&root, Some(&m2)).unwrap();
        assert_ne!(ctime_before, m3.files["src/main.rs"].ctime_ns);
        assert_ne!(m2.files["src/main.rs"].sha256, m3.files["src/main.rs"].sha256);
        let diff = diff_manifest(&m2, &m3);
        assert_eq!(diff.modified, vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn test_manifest_same_size_preserved_mtime_still_detected() {
        #[cfg(unix)]
        {
            let root = test_project("ctime_mtime");
            let target = root.join("src/main.rs");
            let m1 = build_manifest(&root, None).unwrap();
            let sha_before = m1.files["src/main.rs"].sha256.clone();

            // Same-length content change, then restore mtime to the original.
            let m1_time = std::fs::metadata(&target).unwrap().modified().unwrap();
            std::fs::write(&target, "fn MAIN() {}\n").unwrap();
            let ts = chrono::DateTime::<chrono::Local>::from(m1_time).format("%Y%m%d%H%M.%S");
            let _ = std::process::Command::new("touch")
                .args(["-t", &ts.to_string()])
                .arg(&target)
                .status();

            // mtime is restored but ctime moved → fast path must NOT trust the
            // old sha; the content change must surface as modified.
            let m2 = build_manifest(&root, Some(&m1)).unwrap();
            assert_ne!(sha_before, m2.files["src/main.rs"].sha256);
            let diff = diff_manifest(&m1, &m2);
            assert_eq!(diff.modified, vec!["src/main.rs".to_string()]);
        }
    }

    #[test]
    fn test_classify_anchor_absolute_path_detects_removal() {
        let app_dir = std::env::temp_dir().join("rlm_cache_abs_anchor");
        let root = app_dir.join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        for i in 0..10 {
            std::fs::write(root.join(format!("src/file_{}.rs", i)), format!("fn f{}() {{}}\n", i)).unwrap();
        }

        let store = RlmCacheStore::new(&app_dir);
        let manifest = build_manifest(&root, None).unwrap();
        let mut brief = ResearchBrief {
            summary: "s".into(),
            key_files: vec![],
            ..Default::default()
        };
        // The model wrote an ABSOLUTE key file path.
        brief.key_files.push(crate::agent::swarm::BriefFile {
            path: root.join("src/file_0.rs").to_string_lossy().to_string(),
            why: "anchor".into(),
            key_symbols: vec![],
        });
        let audit = ContextAudit { complete: true, summary: "ok".into(), missing: vec![] };
        let inv = KernelInventory { loaded_paths: vec![], generated_at: Local::now() };
        store.save(&root, &brief, &audit, "d", &manifest, &inv).unwrap();

        let cached = store.load(&root).unwrap();
        let empty_diff = diff_manifest(&manifest, &build_manifest(&root, Some(&manifest)).unwrap());
        // Unchanged tree → Sufficiency even with an absolute anchor path.
        assert_eq!(
            classify_cache_state(&root, Some(&cached), Some(&empty_diff), 0.3, Duration::days(30)),
            CacheDecision::Sufficiency
        );

        // Remove the anchored file → Fresh.
        std::fs::remove_file(root.join("src/file_0.rs")).unwrap();
        let removed_manifest = build_manifest(&root, Some(&manifest)).unwrap();
        let removed_diff = diff_manifest(&manifest, &removed_manifest);
        assert_eq!(
            classify_cache_state(&root, Some(&cached), Some(&removed_diff), 0.3, Duration::days(30)),
            CacheDecision::Fresh
        );

        let _ = std::fs::remove_dir_all(&app_dir);
    }
}
