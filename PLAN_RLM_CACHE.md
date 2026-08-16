# 📋 PLAN: RLM Research Cache + Sufficiency Flow + Time-Awareness

> Status: **DIIMPLEMENTASIKAN — Fase 0–6 selesai; Fase 7 (cross-project tool) tertunda/opsional**
> Tanggal: 2026-08-12T21:10+07:00
> Scope: `src-tauri/src/agent/` (Rust backend), tanpa perubahan frontend wajib

---

## 1. LATAR BELAKANG & MASALAH

Saat ini setiap chat baru menjalankan **Phase 0 RLM dari nol** (`swarm.rs:188-409`):
RLM Model mengeksplorasi project hingga 12 turn → `submit_brief` → RLM Verifier audit.
Brief tervalidasi hanya hidup di context window swarm run itu — tidak pernah ditulis ke disk.

Akibatnya:
1. **Re-research berulang** untuk project yang sama (boros token model+verifier).
2. Riset lintas sesi tidak bisa dipakai ulang, padahal kernel Python sendiri persisten
   (`static RLM_MANAGER`, `rlm_kernel.rs:561`) — datanya hidup tapi "memori"-nya hilang.
3. Model tidak sadar waktu: tidak bisa menilai "riset ini sudah 3 hari lalu, mungkin basi".

## 2. TUJUAN

1. **Cache riset RLM per-project**, disimpan DI LUAR folder project (di `app_data_dir`)
   agar tidak mengotori repo dan mudah diakses antar-project.
2. **Sufficiency flow**: brief lama tidak dipakai buta — RLM Model mengevaluasi
   "cukup untuk request ini?" → cukup = langsung `submit_brief`; kurang = cari gap saja.
3. **Time-awareness**: timestamp pada data cache, per-pesan, dan system prompt
   (di akhir, agar prefix context-cache tetap efektif).

## 3. KEPUTUSAN DESAIN YANG SUDAH TETAP

| Keputusan | Pilihan | Alasan |
| :--- | :--- | :--- |
| Algoritma hash | **SHA256** (bukan MD5) | Sudah konsisten di `checkpoint_manager.rs:154` & `_rlm_load` (`rlm_kernel.rs:237`); collision-free; CPU negligible untuk source files. Crate `sha2` sudah ada (`Cargo.toml:57`). |
| Lokasi cache | `<app_data_dir>/rlm_cache/` | Sudah kanonik (`state.rs:72` `require_app_data_dir`); di luar project; satu induk dengan `chat_history/`, `history/`, `gateway_audit/`. |
| Key per project | **hash path project_root** | Stabil terhadap rename folder; path asli tetap disimpan di `meta.json` untuk lookup manusia. |
| Manifest | `{size, mtime_ns, sha256}` per file | Fast-path: hash ulang hanya jika size/mtime beda. |
| Invalidasi memori kernel | mtime **+ sha256** | Fix bug `rlm_kernel.rs:233` (sha256 dihitung tapi tak dipakai → `cp -p`/`touch -r` bikin cache basi). |
| Resolusi waktu di system prompt | **menit** (bukan detik) | Detik membuat setiap request unik & merusak reuse cache antar request beruntun; menit cukup untuk time-awareness. |
| Cross-project retrieval | Tool Rust `rlm_related_briefs` (fase opsional) | Tidak perlu ekspansi allowlist kernel; mudah diaudit via gateway audit. |
| Walk file | crate `ignore` (sudah di `Cargo.toml:20`) | Hormati `.gitignore` secara otomatis. |

## 4. ARSITEKTUR PENYIMPANAN

```
<app_data_dir>/rlm_cache/
├── index.json                      # registry global (untuk cross-project fase 7)
│   {
│     "projects": [
│       {
│         "path": "/Users/.../kuda-ide",       # project_root asli
│         "hash": "ab12cd34",                  # key folder
│         "last_used": "2026-08-12T20:51+07:00",
│         "last_research_at": "2026-08-12T19:40+07:00",
│         "brief_summary": "Tauri IDE dengan swarm RLM..."
│       }
│     ]
│   }
│
└── ab12cd34/                       # = sha256(project_root)[..8] (atau 16 hex chars)
    ├── meta.json                   # {
    │                                 project_root, schema_version: 1,
    │                                 created_at, last_used
    │                               }
    ├── manifest.json               # snapshot pohon file (TIDAK berisi konten):
    │   {
    │     "generated_at": "2026-08-12T20:51:00+07:00",
    │     "file_count": 143,
    │     "files": {
    │       "src-tauri/src/agent/swarm.rs": {
    │         "size": 45821,
    │         "mtime_ns": 1755006600000000000,
    │         "sha256": "9f3a..."
    │       }
    │     }
    │   }
    ├── brief.json                  # ResearchBrief tervalidasi (struct swarm.rs)
    ├── audit.json                  # ContextAudit terakhir
    ├── brief_digest.txt            # output format_brief_digest() — siap inject
    └── inventory.json              # daftar file yang pernah di-load ke kernel Python
        { "loaded_paths": ["src/main.rs", ...], "generated_at": "..." }
```

Aturan:
- **`manifest.json` tidak pernah menyimpan konten file** — hanya metadata. Konten di-reload
  dari project saat pre-warm. Ini membuat cache tetap kecil dan tidak jadi sumber basi.
- Semua field waktu: **ISO 8601 + offset timezone lokal** (`chrono::Local`), contoh
  `2026-08-12T20:51+07:00` — supaya model bisa hitung selisih hari/jam.
- File korup / tak ter-parse ⇒ **fail-open**: anggap cache tidak ada, riset penuh.
  Cache tidak boleh pernah memblokir chat.

## 5. FASE IMPLEMENTASI

---

### FASE 0 — Fix prasyarat (wajib duluan, kecil)

**0a. Fix invalidasi `_rlm_load`** — `rlm_kernel.rs:220-246` (`MEMO_PY`)
```python
def _rlm_load(path):
    ...
    key = os.path.realpath(path)
    st = os.stat(path)
    entry = _rlm_index.get(key)
    if entry is not None and entry['mtime'] == st.st_mtime and entry['size'] == st.st_size:
        return entry['content']          # fast path: mtime+size cukup
    with open(path, 'r') as f:
        content = f.read()
    sha = hashlib.sha256(content.encode('utf-8', errors='replace')).hexdigest()
    if entry is not None and entry['sha'] == sha:
        # konten sama walau mtime berubah (cp -p, rsync --times) → perbarui meta, jangan re-proses
        entry['mtime'] = st.st_mtime; entry['size'] = st.st_size
        return entry['content']
    _rlm_index[key] = {'mtime': st.st_mtime, 'size': st.st_size,
                       'sha': sha, 'content': content, 'loaded_at': time.time()}
    return content
```
- Test baru di `rlm_kernel.rs` (mod tests): `cp -p`/touch mtime tidak memicu reload
  yang salah; perubahan konten tetap terdeteksi.

**0b. Stabilkan `Message` untuk timestamp** — `llm_client.rs:23`
```rust
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Local>>,   // backward-compat dengan JSON lama
}
```
- `Message::user()` / `::assistant()` tetap tanpa timestamp; timestamp diisi oleh
  `ChatHistoryManager::append_message` (`chat_history.rs:102`) saat persist.
- Format lama tanpa field `created_at` tetap ter-load (`serde(default)`).

---

### FASE 1 — Modul cache store (`src-tauri/src/agent/rlm_cache.rs`, BARU)

Struktur Rust:
```rust
pub struct FileManifestEntry { pub size: u64, pub mtime_ns: i64, pub sha256: String }
pub struct FileManifest {
    pub generated_at: DateTime<Local>,
    pub file_count: usize,
    pub files: HashMap<String, FileManifestEntry>,   // key: relative path
}
pub struct ProjectCacheMeta { pub schema_version: u32, pub project_root: String,
    pub created_at: DateTime<Local>, pub last_used: DateTime<Local> }
pub struct KernelInventory { pub loaded_paths: Vec<String>, pub generated_at: DateTime<Local> }

pub struct RlmCacheStore { root: PathBuf }           // <app_data_dir>/rlm_cache
impl RlmCacheStore {
    pub fn new(app_data_dir: &Path) -> Self;
    pub fn project_key(project_root: &Path) -> String;          // hex sha256[..16]
    pub fn load(&self, project_root: &Path) -> Option<ProjectCache>; // fail-open: None jika korup
    pub fn save(&self, ...) -> Result<()>;                        // atomic: tulis ke .tmp lalu rename
    pub fn load_brief(&self, ...) -> Option<(ResearchBrief, ContextAudit, String /*digest*/)>;
}
```
Registrasi modul di `agent/mod.rs`.

Test: round-trip save/load, korup JSON → None, atomic write (file `.tmp` tidak tersisa).

---

### FASE 2 — Manifest builder + diff (masih di `rlm_cache.rs`)

```rust
pub fn build_manifest(project_root: &Path, old: Option<&FileManifest>) -> Result<FileManifest>;
pub struct ManifestDiff {
    pub added: Vec<String>, pub removed: Vec<String>,
    pub modified: Vec<String>, pub unchanged_count: usize,
    pub changed_ratio: f32,          // (added+removed+modified) / total
}
pub fn diff_manifest(old: &FileManifest, new: &FileManifest) -> ManifestDiff;
```
- Walk pakai `ignore::WalkBuilder` (hormati `.gitignore`, `Cargo.toml:20`).
- Parallelisme: `WalkBuilder.threads(4)` (jika perlu; ukur dulu).
- **Fast-path hash**: file dengan `(size, mtime_ns)` sama dengan manifest lama → salin
  sha256 lama, tidak dibaca dari disk. Hash ulang hanya yang beda.
- Skip file biner (deteksi NUL di 8KB pertama) & file > ambang ukuran (mis. 2 MB) —
  tidak relevan untuk riset kode; catat di manifest sebagai `{size, mtime_ns, sha256: null}`?
  → Keputusan: skip total, tidak masuk manifest.

Test: buat temp-tree → build → ubah 1 file → build lagi → diff benar; file `.gitignore`
ter-exclude; hanya file berubah yang di-hash ulang (assert lewat counter internal).

---

### FASE 3 — Tulis cache setelah riset sukses (integrasi `swarm.rs`)

Hook: setelah `brief_text` final ditetapkan (`swarm.rs:372-385`, sebelum push ke shared
di `swarm.rs:401-408`) dan hanya jika `audit.complete == true`:

```rust
// Pseudocode di run_swarm, setelah format_brief_digest(&brief, &audit):
let store = RlmCacheStore::new(&app_data_dir);
store.save_project_cache(&project_root, &brief, &audit, &digest, &manifest)?;
store.update_index(&project_root, &brief)?;          // index.json
```
- Simpan juga `inventory.json`: daftar path yang pernah di-`_rlm_load`/dibaca pada
  run ini (dikumpulkan dari event tool-call `rlm_python`/`batch_file_read` yang sudah
  ada di loop — atau query kernel: `list(_rlm_index.keys())`).
- Gagal tulis cache = warn log saja, tidak membatalkan chat.

---

### FASE 4 — Sufficiency flow (inti; integrasi `swarm.rs` Phase 0)

Sisipkan **sebelum** loop `for rlm_round in 0..MAX_RLM_ROUNDS` (`swarm.rs:194`):

```
run_swarm mulai
├── cache = RlmCacheStore.load(project_root)
├── manifest_baru = build_manifest(project_root, cache.manifest)
├── diff = diff_manifest(cache.manifest, manifest_baru)
│
├── KASUS A: cache hit & diff kosong & brief.key_files masih valid
│     → mode SUFFICIENCY-CHECK:
│       1. pre-warm kernel (Fase 5)
│       2. prompt khusus ke RLM Model (bukan eksplorasi terbuka):
│          "Riset sebelumnya (generated_at X, N menit lalu): <brief_json>.
│           Data sudah di-load ke kernel. Request user: <query>.
│           Evaluasi kecukupan: CUKUP → submit_brief (boleh refine minor).
│           KURANG → kumpulkan hanya gap-nya, lalu submit_brief."
│       3. max_turns dipangkas (mis. 4, bukan 12) — budget untuk fast-path
│       4. Verifier TETAP audit (safety net salah-judge).
│     → jika Verifier bilang incomplete → fallback ke KASUS B (rilis ronde lanjutan)
│
├── KASUS B: cache hit & ada perubahan (diff kecil, < ambang)
│     → mode INCREMENTAL:
│       pre-warm + prompt: "Riset sebelumnya ada (tgl X). File berubah sejak itu:
│       [list diff]. Kumpulkan/refresh hanya bagian yang relevan dengan request."
│       max_turns penuh (12), start dengan brief lama sebagai referensi.
│
└── KASUS C: cache miss / perubahan masif (changed_ratio > 0.30) /
             key_files brief berubah-hilang / brief > umur maksimum (mis. 30 hari)
      → mode FRESH: Phase 0 normal seperti sekarang.
      (brief lama, jika ada, tetap disisipkan sebagai "referensi usang" —
       dengan label eksplisit agar tidak anchoring.)
```

Detail keputusan:
- **Ambang fresh** (`changed_ratio > 0.30`) dan **umur maksimum brief** (30 hari)
  dibuat `const` di `swarm.rs` agar mudah tuning.
- **Anti-anchoring**: semua brief lama yang di-inject diberi header:
  `"[PRIOR RESEARCH — {generated_at}, {rel}, data session sebelumnya. Verifikasi sebelum percaya.]"`
- Emit `AgentEventKind` baru (mis. `PhaseStarted` label
  `"RLM Model: sufficiency check (cached)"`) agar UI menampilkan tahap yang benar —
  tidak perlu variant event baru, cukup label.
- Token tracking tidak berubah (`total_tokens` tetap accumulate normal).

Test (unit, tanpa LLM — pakai mock provider jika ada pola mock di codebase, jika tidak
test hanya logika klasifikasi A/B/C):
- `classify_cache_state()` → fungsi murni
  `(Option<ProjectCache>, ManifestDiff, age) -> CacheDecision { Sufficiency | Incremental | Fresh }`
  yang di-test terpisah. Ekstrak logika keputusan jadi fungsi murni = mudah di-test.

---

### FASE 5 — Kernel pre-warm & inventory (`rlm_kernel.rs` + `swarm.rs`)

Tujuan: saat chat baru mulai dengan cache valid, kernel langsung berisi data sehingga
RLM Model tidak perlu re-read file-file yang sudah dikenal.

```rust
// Di RlmKernelManager (rlm_kernel.rs:443):
pub async fn prewarm(&self, project_root: &Path, paths: &[String]) -> Result<usize> {
    let mut guard = self.get_or_spawn(project_root).await?;
    // exec snippet: for p in paths: try _rlm_load(os.path.join(root,p)) except: pass
    // return jumlah yang berhasil di-load
}
```
- Pre-warm hanya file yang: (a) ada di `inventory.json`/`brief.key_files`, DAN
  (b) masih ada & unchanged menurut manifest baru. File berubah tidak di-prewarm
  (biar Model baca versi baru secara eksplisit).
- Batas: maks 50 file & total 2 MB konten (konstanta), supaya pre-warm tidak lama.
- Pre-warm timeout terpisah (mis. 20 detik) — gagal = lanjut tanpa pre-warm.
- Setelah riset selesai, snapshot isi `_rlm_index.keys()` → `inventory.json` (Fase 3).

Test: prewarm di temp-dir dengan 2 file → `_rlm_index` berisi 2 entri; file hilang
diskip tanpa error.

---

### FASE 6 — Time-awareness

**6a. Block `<environment>` di EKOR system prompt** — `prompt_composer.rs`
(`compose_system_prompt:52` & `compose_role_prompt:84`)
```rust
fn compose_env_block(project_root: &Path, prior_research: Option<DateTime<Local>>) -> String {
    // <environment>
    // Current time: 2026-08-12T20:54+07:00          ← resolusi MENIT, Local TZ
    // Project: /path/ke/project
    // Prior research: 46 minutes ago (2026-08-12T20:08+07:00) | none
    // </environment>
}
```
- Ditambahkan di **akhir** prompt (menjaga prefix cache).
- Helper `format_relative(now - ts)` → "just now / 12 minutes ago / 3 hours ago / 2 days ago".
- Test: assert substring (bukan exact match) supaya tidak pecah tiap menit.

**6b. Timestamp per pesan untuk LLM request** — di build pesan (`commands/agent.rs`
sekitar `agent_chat`/`agent_swarm_chat`, atau helper baru di `llm_client.rs`):
- User message diberi prefix `[2026-08-12T20:54+07:00]` saat **dibangun untuk request**,
  menggunakan `created_at` tersimpan (jika ada), bukan waktu sekarang — supaya pesan
  lama tidak berubah & prefix history stabil untuk cache.
- Pesan tanpa `created_at` (data lama) → tanpa prefix, aman.

**6c. Timestamp di AgentEvent** — `orchestrator.rs` (`AgentEventKind`):
- Tambah field `at: DateTime<Local>` (serde default) pada event `PhaseStarted`/
  `PhaseCompleted` → UI bisa menampilkan durasi tiap tahap; log audit punya timeline.
- Frontend boleh menampilkan durasi nanti (opsional, tidak wajib di plan ini).

**6d. Metadata cache** — semua file di Fase 1/3 membawa `generated_at` ISO+TZ.
Saat inject brief lama ke prompt, selalu sertakan "N menit/jam/hari lalu".

---

### FASE 7 — Cross-project retrieval (OPSIONAL / follow-up)

- `index.json` sudah ditulis sejak Fase 3 — Fase ini hanya menambah reader.
- Tool baru `rlm_related_briefs` di `tool_registry.rs`:
  - params: `{ "query": "auth login", "limit": 3 }`
  - cari substring keyword di `brief_summary` (index.json) + nama path
  - return: daftar `{project_path, last_research_at, brief_digest (truncated)}`
  - daftar allowed tools role `rlm_model` (`roles.rs`) ← tambahkan di sana.
- Tidak disambungkan ke kernel Python (baca via Rust) → tanpa perubahan allowlist.
- Keamanan: hanya membaca brief sendiri milik app; tetap di dalam `app_data_dir`.

## 6. BUG / PERBAIKAN YANG DIBAWA SEKALIAN

| Lokasi | Masalah | Fix |
| :--- | :--- | :--- |
| `rlm_kernel.rs:233` | sha256 dihitung tapi tidak dipakai invalidasi (hanya mtime) | Fase 0a |
| `chat_history.rs` | Pesan tanpa timestamp → model tak bisa rekonstruksi timeline antar sesi | Fase 0b + 6b |

## 7. RISIKO & MITIGASI

| Risiko | Mitigasi |
| :--- | :--- |
| Brief lama anchoring (model percaya data basi) | Verifikasi key_files vs manifest sebelum inject; label eksplisit "verifikasi sebelum percaya"; ambang umur 30 hari → FRESH |
| Sufficiency salah-judge "cukup" | Verifier tetap audit setiap run; incomplete → fallback KASUS B |
| Cache korup / schema bergeser | `schema_version` di meta.json; semua load fail-open → None |
| Pre-warm lambat untuk project besar | Batas 50 file / 2 MB; timeout 20s; gagal = lanjut tanpa pre-warm |
| Project diganti di tengah sesi | `get_or_spawn` sudah respawn saat project berubah (`rlm_kernel.rs:528-536`); cache ikut ter-key ulang otomatis |
| Cache tumbuh tak terbatas | `index.json` punya `last_used` → nanti bisa ditambah evict LRU (di luar scope plan ini, catat sebagai TODO) |
| Test determinism | Test prompt pakai substring assert; waktu di-mock lewat parameter fungsi (jangan panggil `Local::now()` di dalam logika murni) |

## 8. URUTAN KERJA & ESTIMASI

| # | Fase | Deliverable | Ukuran |
| :--- | :--- | :--- | :--- |
| 1 | Fase 0 | Fix `_rlm_load` + field `created_at` | S |
| 2 | Fase 1 | `rlm_cache.rs` store | M |
| 3 | Fase 2 | Manifest builder + diff | M |
| 4 | Fase 3 | Tulis cache pasca-riset sukses | S |
| 5 | Fase 5 | Pre-warm kernel | S |
| 6 | Fase 4 | Sufficiency flow di swarm (inti) | L |
| 7 | Fase 6 | Time-awareness (composer, pesan, event) | M |
| 8 | Fase 7 | Cross-project tool (opsional) | S |

Urutan 1→7 sengaja begitu: store siap dulu, baru alur swarm diubah. Fase 7 independent.

## 9. CHECKLIST VERIFIKASI AKHIR

- [ ] `cargo test` di `src-tauri` lulus (test baru: memo fix, cache store, manifest diff,
      `classify_cache_state`, prewarm, substring prompt).
- [ ] `cargo clippy` bersih.
- [ ] Manual: chat pertama pada project bersih → riset penuh; chat kedua (tanpa ubah
      file) → tercetak "sufficiency check" dan selesai jauh lebih cepat.
- [ ] Manual: edit 1 file → chat baru → hanya bagian terpengaruh yang diriset ulang.
- [ ] Manual: hapus folder `rlm_cache/` → app kembali ke perilaku riset penuh tanpa error.
- [ ] Cek `<app_data_dir>/rlm_cache/index.json` ter-update tiap riset sukses.
- [ ] JSON chat history lama tetap bisa dimuat (backward compat `created_at`).

## 10. FILE YANG DISENTUH

**Baru**
- `src-tauri/src/agent/rlm_cache.rs`

**Diubah**
- `src-tauri/src/agent/mod.rs` — registrasi modul `rlm_cache`
- `src-tauri/src/agent/swarm.rs` — hook tulis cache, sufficiency flow, ambang, event label
- `src-tauri/src/agent/rlm_kernel.rs` — fix `MEMO_PY`, `prewarm()` di `RlmKernelManager`
- `src-tauri/src/agent/llm_client.rs` — `Message.created_at`, helper prefix timestamp
- `src-tauri/src/agent/chat_history.rs` — isi `created_at` saat append
- `src-tauri/src/agent/prompt_composer.rs` — env block waktu di ekor prompt
- `src-tauri/src/agent/orchestrator.rs` — `at` timestamp di event
- `src-tauri/src/agent/roles.rs` — (Fase 7) tool baru untuk RLM Model
- `src-tauri/src/agent/tool_registry.rs` — (Fase 7) `rlm_related_briefs`
- `src-tauri/src/commands/agent.rs` — lewatnya info `prior_research` ke composer (jika perlu)

**Tidak disentuh (wajib)**
- Frontend (`src/`) — tidak ada perubahan wajib; label event baru tampil otomatis.

---

## 11. LOG IMPLEMENTASI (2026-08-12)

| Fase | Status | Ringkasan |
| :--- | :--- | :--- |
| 0a | ✅ | `_rlm_load` kini invalidasi mtime+size+sha256 (`rlm_kernel.rs` MEMO_PY); test `test_rlm_python_memo_detects_change_with_preserved_mtime`. |
| 0b | ✅ | `Message.created_at` (serde default, backward compat); diisi oleh `chat_history.rs::append_message`. |
| 1+2 | ✅ | `src-tauri/src/agent/rlm_cache.rs` baru: `RlmCacheStore` (atomic write), `FileManifest`, `KernelInventory`, `RlmCacheIndex`, `build_manifest` (ignore crate, fast-path hash), `diff_manifest`, `classify_cache_state` (pure). |
| 3 | ✅ | `swarm.rs` menulis cache (brief+audit+digest+manifest+inventory) setelah riset tervalidasi (`audit.complete`), non-fatal. |
| 4 | ✅ | Sufficiency/Incremental/Fresh di `swarm.rs` Phase 0; sufficiency budget 4 turn; verifier tetap safety net; brief basi di-inject berlabel STALE (anti-anchoring). |
| 5 | ✅ | `RlmKernelManager::prewarm` (max 50 file, best-effort) + `inventory_snapshot` (path relatif); test prewarm. |
| 6 | ✅ | Env block `<environment>` (Current time menit-resolusi + Project) di ekor `compose_system_prompt` & `compose_role_prompt`; prefix `[ISO8601]` pada user message saat build request (`Message::stamped_for_request`); digest brief diberi timestamp. |
| 7 | ⏳ | Cross-project `rlm_related_briefs` tool belum dibuat — fondasi `index.json` sudah ditulis setiap save. |
| Validasi | ✅ | `cargo test --lib`: 53 lulus; `cargo build` sukses; clippy tanpa warning baru. |

**Perbaikan lanjutan (2026-08-12, round 2):**
- **ctime di manifest** — `FileManifestEntry.ctime_ns` (unix; fallback mtime di platform lain). Fast-path kini cek `(size, mtime, ctime)`; setiap write mengubah ctime walau mtime di-restore (`cp -p`/`touch -r`), menutup blind spot "size+mtime identik". Legacy manifest (`ctime_ns==0`) di-re-hash sekali. Test: `test_manifest_same_size_preserved_mtime_still_detected`, `test_manifest_ctime_changes_but_sha_stable`.
- **Anchor path absolut** — `classify_cache_state` kini menerima `project_root` dan memakai `normalize_relative` (canonicalized match → string-prefix strip terhadap root raw *dan* canonical → fallback). Memperbaiki deteksi key_file yang ditulis model sebagai path absolut, termasuk saat file sudah dihapus dan root bersymlink (`/var` vs `/private/var`). Test: `test_classify_anchor_absolute_path_detects_removal`.
- 59 test lulus; clippy tanpa warning baru.

**Catatan implementasi yang menyimpang dari rencana:**
- Cache yang sebagian korup di-load fail-open per-field (`load` tetap Some, field None) → `classify_cache_state` mengembalikan Fresh. Lebih tangguh daripada gagal total.
- Fase 6d (field `at` di AgentEvent) dilewati — mengubah kontrak JSON frontend tanpa kebutuhan fungsional; waktu tahap sudah terlihat via label + digest.
- Ambang & umur bisa di-tuning via konstanta `MAX_BRIEF_AGE_DAYS`, `CHANGED_RATIO_THRESHOLD`, `SUFFICIENCY_MAX_TURNS` di `swarm.rs`.
