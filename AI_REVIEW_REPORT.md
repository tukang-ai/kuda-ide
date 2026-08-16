# Laporan Review & Handoff — KudaIDE

Dokumen ini untuk **AI lain / reviewer lanjutan** yang akan mencari bug tambahan.
Baca dulu bagian "Sudah Diperbaiki" dan "Sengaja Dibiarkan" agar tidak melapor ulang
temuan yang sama, lalu fokus pada bagian "Area Prioritas Pencarian Bug Lanjutan".

---

## 1. Ringkasan Proyek

- **Stack**: Tauri v2 (Rust backend) + React 18 / TypeScript / Vite / Zustand / Monaco / xterm.js
- **Lokasi**: `src/` (frontend), `src-tauri/src/` (backend Rust)
- **Fungsi**: IDE desktop dengan editor, terminal PTY, file explorer, search, agentic AI
  (swarm multi-agent: RLM research → Thinker direction → Planning Writer → Reviewer →
  Plan Gate → Executor → Executor Reviewer), gateway keamanan 5-layer, Kuda Hub subscription.
- **Arsitektur singkat**:
  - Frontend: `src/store/*` (Zustand), `src/components/*`, `src/lib/ipc.ts` (wrapper invoke)
  - Backend: `commands/*` (IPC), `agent/*` (swarm, orchestrator, tools, providers, kernel RLM),
    `gateway/*` (rate limiter, intent guard, token, vault), `indexer/*` (search + AST),
    `file_system/*`, `terminal/*`, `diff_engine/*`, `security.rs` (PathGuard)

### Perintah build / test (jalankan dari direktori proyek)
```bash
# Frontend
node node_modules/typescript/bin/tsc --noEmit        # typecheck
node node_modules/vitest/vitest.mjs run              # unit test (8 test)

# Backend (dari src-tauri/)
/Users/macmini/.cargo/bin/cargo check --all-targets  # cek compile (0 warning)
/Users/macmini/.cargo/bin/cargo test --lib           # unit test (109 test)

# Jalankan app (dev)
npm run tauri dev
```

**Catatan**: pakai `node node_modules/...` langsung — `npx` hang di filesystem lambat (external SSD via mounty).

---

## 2. Status Baseline Saat Ini (sudah terverifikasi)

- `cargo test --lib`: **109 passed / 0 failed**
- `cargo check --all-targets`: **0 error, 0 warning**
- `tsc --noEmit`: bersih
- `vitest`: **8 passed**

---

## 3. Bug yang SUDAH Diperbaiki (jangan dilapor ulang)

### 3.1 Keamanan (kritis)
| # | Bug | File | Ringkasan perbaikan |
|---|-----|------|---------------------|
| 1 | Bypass sandbox Python RLM | `src-tauri/src/agent/rlm_kernel.rs` | (a) `os.environ` disanitasi (API key/token tidak terlihat); (b) `os.kill/killpg/abort/_exit` + builtins `exit/quit` diblokir; (c) probe metadata `os.stat`/`os.path.*` di-scope-check; (d) `_rlm_allowlist` jadi container immutable (`_rlm_Allowlist`, tanpa metode mutasi); (e) `import builtins` ditolak; (f) `__builtins__` per-exec = dict-copy tanpa `open`/`eval`/`exec`; (g) `_rlm_install_guard()` dijalankan ulang sebelum tiap `execute_user_code` (rollback mutasi persisten). |
| 2 | Bypass `rm` guard (flag `-r` setelah target) | `src-tauri/src/agent/tool_registry.rs:449` | `rm -f /etc -r` kini terdeteksi (scan flag recursive di seluruh token setelah `rm`, order-independen). |
| 3 | `terminal_kill` tidak membunuh shell | `pty_manager.rs` + `multiplexer.rs` | Child process disimpan & di-`kill`+`wait`; `kill_session` benar-benar terminasi; command baru `terminal_list`/`terminal_close_all`; session mati di-reap. |
| 4 | `open_external_url` tanpa validasi skema | `src-tauri/src/commands/project.rs` | Hanya `http://`/`https://` yang diteruskan ke OS `open`. |
| 5 | `terminal_spawn` cwd tanpa PathGuard | `src-tauri/src/commands/terminal.rs` | `cwd` divalidasi `validate_path_in_scope` terhadap project root. |
| 6 | Base URL provider tidak divalidasi | `src-tauri/src/agent/roles.rs` + `hub_session.rs` | Semua provider (bukan hanya kuda_hub) divalidasi: https, atau http hanya di loopback. |

### 3.2 Fungsional (tinggi)
| # | Bug | File | Ringkasan perbaikan |
|---|-----|------|---------------------|
| 7 | Resume swarm dari checkpoint `Direction` tidak menjalankan ulang fase direction | `src-tauri/src/agent/swarm.rs` | `resume_direction` menyalakan ulang Phase 0.5 (Thinker conclusion + gate) saat checkpoint `ResumePhase::Direction`. |
| 8 | Token accounting rusak setelah resume (`tokens_out`/`cached_in` = 0) | `swarm.rs` | `RunCheckpoint` kini menyimpan `tokens_out`/`cached_in`; total resume dipertahankan. |
| 9 | `MAX_OUTPUT_TOKENS=1_000_000` menyebabkan 400 pada Gemini/relay | `llm_client.rs` + `orchestrator.rs` | `max_tokens: None` → provider memakai cap masing-masing. |
| 10 | Approval external-access tidak bisa dibatalkan (blokir 5 menit) | `tool_registry.rs` | `tokio::select!` dengan `ctx.cancel.notified()`. |
| 11 | Gate direction "ubah" dianggap "disetujui" | `swarm.rs` | Loop revisi terbatas (`MAX_DIRECTION_REVISIONS=2`), fail-closed setelah batas. |
| 12 | Plan gate revise/review tidak menyinkronkan `shared` | `swarm.rs` | Push `[PLAN]` baru setelah revisi/review; tidak lagi `shared = revise_outcome.history`. |
| 13 | `grep_search` mengabaikan `case_sensitive` | `tool_registry.rs` + `indexer/search.rs` | `RegexMatcherBuilder::case_insensitive(!case_sensitive)`. |
| 14 | `multi_replace_file` clamp baris di luar file secara diam-diam | `tool_registry.rs` | Baris di luar file kini ditolak dengan error eksplisit. |
| 15 | Collision temp-write (`.tmp_write` sama) antar run konkuren | `src-tauri/src/file_system/io.rs` | Nama temp unik per tulis (`tmp_write_{uuid}`). |
| 16 | Direct chat mengekspos tool swarm-only | `orchestrator.rs` | `submit_review_directions` & `request_rlm_research` ikut difilter. |
| 17 | Rate limiter token bucket tidak dijalankan (dead field) | `src-tauri/src/gateway/rate_limiter.rs` | Bucket kini diisi ulang (rate per detik, burst-capped) dan dicek pakai estimasi token request. |
| 18 | Refresh hub menimpa master token | `hub_session.rs` | `save_hub_credentials` mempertahankan master token lama. |
| 19 | Checkpoint resume tidak atomik | `swarm.rs` | `write_checkpoint` pakai tmp + rename. |
| 20 | `transcript.lock().unwrap()` panic pada poisoned mutex | `swarm.rs` | `.lock().map(\|mut c\| c.finish()).unwrap_or_default()`. |
| 21 | CSP memblokir fetch ke `http://localhost:8090` | `src-tauri/tauri.conf.json` | `connect-src` ditambah `http://localhost:8090`, `127.0.0.1:8090`, `ws://localhost:8090`. |

### 3.3 Frontend / fitur yang kurang
| # | Item | File | Ringkasan |
|---|------|------|-----------|
| 22 | Badge "No API key" tidak mendeteksi provider kustom | `src/store/agent.ts` + `TitleBar.tsx` | `checkKey()` kini juga cek `providerList().has_key`. |
| 23 | Terminal leak saat panel ditutup | `TerminalPanel.tsx` | Unmount → `terminal_close_all`; poll `terminal_list` untuk reap sesi mati. |
| 24 | Tombol "@ context" mati (tanpa onClick) | `AgentPanel.tsx` | Kini melampirkan file aktif sebagai konteks ke prompt. |
| 25 | Search Replace adalah dead UI | `SearchPanel.tsx` + `indexer/search.rs` + `commands/indexer.rs` | Backend `search_replace` + tombol "Replace All" di UI. |
| 26 | Tidak ada dirty-tab guard | `src/store/workspace.ts` | `closeTab` memakai `window.confirm` bila ada perubahan belum disimpan. |
| 27 | Layout tidak dipersist | `src/store/layout.ts` | Sidebar/terminal/agent state disimpan ke localStorage. |
| 28 | File watcher tidak pernah dipakai (dead code) | `watcher.rs` + `commands/fs.rs` + `store/workspace.ts` | `fs_watch_start` + `watchProject()`: tab open reload live, explorer refresh. |
| 29 | Badge versi tidak konsisten | `TitleBar.tsx` | `v0.2.0` → `v0.1.0` (sesuai package.json). |

---

## 4. Item yang DIBIARKAN / BELUM DIPERBAIKI (agar AI lain tidak membuang waktu)

### 4.1 Sengaja dibiarkan (keputusan desain, bukan bug)
1. **Review gate fail-open saat role kehabisan turn** — `swarm.rs` `parse_plan_review`/`parse_review_directions` mengembalikan `(true, …)` bila `submit_*` tidak pernah dipanggil. Ini sengaja agar loop berhenti; dibatasi cap (`MAX_REVIEWER_*`/safety cap). Risiko: plan yang tidak ter-audit bisa lolos.
2. **`ensure_hub_session` menelan error refresh** — `hub_session.rs:276-290` mengembalikan `Ok` saat refresh gagal (hub offline tidak boleh memblokir chat). Konsekuensi: session key expired → 401 di request time.
3. **TOCTOU symlink swap saat write file** — inheren pada file ops; dimitigasi dengan canonicalize pada titik tulis + verify inode di kernel.
4. **Isolasi sempurna sandbox RLM terhadap intropeksi `__closure__`/`__globals__`** — residual teoritis; Python tidak bisa meng-hide originals dari fungsi yang bisa diintrospeksi. Sudah di-hardening 6 lapis. Solusi penuh = subproses terisolasi (perubahan arsitektur besar).

### 4.2 Belum diperbaiki (candidate untuk AI lain — atau fix lanjutan)
1. **`SettingsModal.fetchUsage` tanpa token saat load** — `hubToken` state hanya terisi setelah GitHub sign-in; bila kredensial sudah tersimpan di file (`hub_credentials.json`), fetch usage/plan tidak mengirim `Authorization` → kuota/pricing tidak muncul. (File: `src/components/SettingsModal.tsx:168-186`)
2. **`fetchUsage` dipanggil 2x** (line 186 dan 199 di blok yang sama?) — cek kemungkinan double-fetch.
3. **Session restore** — `projectCurrent` masih tidak dipakai; IDE selalu start di Welcome screen (tidak membuka folder terakhir).
4. **`diffCompute`** masih dead IPC wrapper — tidak ada UI diff/preview sebelum revert.
5. **`agentGetConfig`/`agentDeleteConfig`** dead wrapper di `ipc.ts`.

---

## 5. AREA PRIORITAS PENCARIAN BUG LANJUTAN

### 5.1 `src-tauri/src/agent/swarm.rs` (±4600 baris) — prioritas #1
- **Phase transitions**: setiap `break`/`continue` pada loop `'direction_review`, `loop` execution, fix rounds. Uji: apakah ada path di mana `approved_plan` bisa `None` setelah gate off + resume.
- **Ledger math**: invariants `total_tokens == tokens_in + tokens_out`, `cached_in` accounting setelah resume multi-level (Direction → Planning → Executing).
- **Truncation**: `truncate_chars`/`truncate_middle` pada konteks besar; apakah ada informasi yang hilang diam-diam.
- **Gate**: `MAX_PLAN_GATE_ROUNDS` vs `can_modify`; apakah "execute" setelah cap tetap valid.
- **Handoff parsers**: `parse_plan_doc`, `parse_brief_doc`, `parse_audit_markdown`, `parse_verdict` — edge case markdown malformed, nested lists, fields multiline.
- **Checkpoint/resume**: uji siklus penuh resume di tiap fase; cek apakah `shared` konsisten dengan `final_plan` setelah gate.
- **Cancel**: apakah `cancel.notified()` dicek di SEMUA await (ada `tool_ctx.cancel` di banyak tempat)?

### 5.2 Provider layer — `openai.rs`, `gemini.rs` (belum ada network test)
- **SSE parsing** `openai.rs`: baris `data:` terpecah, `[DONE]`, JSON terpotong di tengah chunk, multiple `data:` dalam satu chunk, `retry` fields, status non-200 dengan body besar.
- **Tool call accumulation**: partial JSON args terpecah antar chunk; `tool_call_id` pairing; flush akhir stream; reasoning_content.
- **Gemini**: format `parts`, functionCall vs functionResponse pairing, empty `inlineData`, struktur `content` role.
- **Error mapping**: apakah 401/403/429/5xx dipetakan benar; apakah body error upstream di-sanitasi (tidak bocor ke log/prompt).
- **Timeouts**: `connect_timeout`/`read_timeout`; apakah stream yang macet (tanpa data, tanpa error) pernah di-kill (cari `timeout` di loop stream).

### 5.3 Orchestrator — `orchestrator.rs`
- **Tool loop**: kondisi terminasi, turn counting (`max_turns`), apakah tool call dengan args malformed di-retry atau di-skip.
- **Retry/backoff**: `stream_with_retry` — exponential backoff, `Retry-After` parse, apakah retry bisa menggandakan input tokens (double-count).
- **`truncate_middle`**: off-by-one di tengah teks (koma di tengah kata), encoding (UTF-8 boundary).
- **Prompt injection**: apakah output tool yang berisi instruksi bisa memodifikasi system prompt (ada sanitasi?).

### 5.4 RLM kernel & cache — `rlm_kernel.rs`, `rlm_cache.rs`
- **Kernel lifecycle**: race di `get_or_spawn` (lock ganda `kernel` vs `allowlist`); respawn saat child mati; `kill_on_drop`; stdout/stderr partial read; kernel hang → timeout.
- **Allowlist sync**: `sync_kernel_allowlist` vs `allowlist_dirty` — apakah ada jendela di mana approval tidak terlihat oleh kernel yang sedang berjalan.
- **Memoisasi**: `_rlm_load` invalidation (mtime+size+sha) — file besar, path symlink, `cp -p`, perubahan saat kernel idle.
- **Scratch dir**: `stage_scratch_file` — sisa file saat crash; permission; UUID collision (sangat kecil).
- **rlm_cache**: atomic write `write_atomic`, `project_hash`, manifest build pada repo besar (perf), race antara `load` dan `save`.

### 5.5 Keamanan / gateway
- **`intent_guard.rs`**: blocked patterns hardcoded ("write a blog post", "translate this article") — apakah memblokir request user yang sah; apakah `required_context_keywords` bisa gagal untuk swarm mode.
- **`ephemeral_token.rs`**: JWT validation defaults (apakah `Validation::default()` memvalidasi `exp` benar), `active_tokens` unbounded (prune hanya saat >4096).
- **`device_fingerprint.rs`**: `System::kernel_version()` sebagai "boot UUID" — apakah berubah antar boot → token invalid?
- **PathGuard**: relatif vs absolut, symlink, case-insensitive FS (macOS), `..` di tengah path, trailing slash.
- **Keychain**: `key_store.rs` fallback env var — apakah secret bisa bocor ke log/env dump.
- **CSP/tauri.conf**: apakah permission Tauri v2 (`capabilities/*.json`) mencakup semua command; apakah ada command berbahaya yang diekspos tanpa approval.

### 5.6 Frontend
- **`src/store/agent.ts`**: dedupe event, urutan event stream (Finished vs PhaseStarted), `resumeTarget` untuk direct-chat (resume tombol muncul untuk run non-swarm?), `StreamTextBuffer` timing (delta hilang saat reset), `buildLiveItems` grouping.
- **`EditorPane.tsx`**: monaco model lifecycle (save race: user mengetik saat `saveFile` berjalan), Cmd+S handler, tab switch saat dirty.
- **`FileExplorer.tsx`**: drag & drop (target sama, folder ke child-nya), rename ke path yang sudah ada, keyboard F2/Delete focus.
- **`SettingsModal.tsx`**: flow GitHub OAuth (polling, error), `agentSaveKey('kuda_hub_master', …)` vs `provider_key.kuda_hub`, switch tab yang membatalkan save.
- **`CheckpointsPanel.tsx`**: restore checkpoint pada file yang sudah di-delete/rename; multiple checkpoint per file.
- **`SearchPanel.tsx`**: `renderMatchSnippet` dengan regex lookahead/global flag; scope 'specific_file' tanpa filter.
- **`App.tsx`**: keybindings (Cmd+J/B/I; klaim Cmd+F di TitleBar tidak ada handler), `PendingExternalAccessList` overlay z-index, init race (project belum terbuka saat `agentConfigGet`).

### 5.7 Diff engine & history
- **`diff_engine/history.rs`**: restore file binary vs teks; line ending CRLF vs LF; file besar (perf); session revert pada file yang berubah di luar sesi.
- **`diff_engine/calculator.rs`**: diff besar (myers), Unicode (astral chars), path dengan karakter aneh.

### 5.8 Terminal
- **`pty_manager.rs`**: `is_base64` field tidak pernah `true` — apakah output non-UTF8 aman (`from_utf8_lossy` mengganti byte); resize race; `write_bytes` vs master lock; multiple reader thread pada PTY yang sama; shell mati → channel `send` error.

---

## 6. Kontrak IPC (backend ↔ frontend) — cek konsistensi

Backend `invoke_handler` (lib.rs) — pastikan SEMUA command di atas sudah ada wrapper di `src/lib/ipc.ts`:

project_open, project_current, open_external_url,
fs_list_dir, fs_read_file, fs_write_file, fs_delete, fs_create_dir, fs_rename, fs_watch_start,
terminal_spawn, terminal_write, terminal_resize, terminal_kill, terminal_list, terminal_close_all,
search_code, search_replace, parse_symbols,
agent_chat, agent_swarm_chat, agent_resume_run, agent_get_config, agent_delete_config,
agent_save_key, agent_has_key, agent_refresh_hub_session, agent_ensure_hub_session,
agent_save_hub_credentials, agent_has_hub_credentials, agent_hub_account, agent_hub_sign_out,
provider_list, provider_save, provider_delete, agent_config_get, agent_config_set,
chat_list_sessions, chat_load_session, chat_delete_session,
agent_approve_external_access, agent_deny_external_access, agent_resolve_plan_decision,
agent_resolve_direction_decision, agent_bind_external_events, agent_cancel_run,
gateway_issue_token, gateway_get_device_hash, gateway_get_usage_stats,
history_list_checkpoints, history_list_sessions, history_revert_session,
history_restore_checkpoint, diff_compute

**Tugas AI lain**: untuk tiap command di atas, cek (a) ada wrapper di `ipc.ts`? (b) nama param Tauri (camelCase) cocok dengan harapan frontend? (c) tipe return frontend cocok dengan struct Rust?

---

## 7. Checklist Verifikasi untuk AI Lain

1. Jalankan `cargo test --lib` dan `tsc --noEmit` dulu — pastikan baseline hijau.
2. Untuk tiap temuan: berikan **file:line**, **ringkasan 1 kalimat**, **severity** (Critical/High/Medium/Low), **kode/path eksploitasi konkret**, dan **usulan fix**.
3. JANGAN mengubah kode tanpa diminta — dokumen ini untuk review, bukan fix.
4. Periksa ulang apakah temuan sudah ada di Bagian 3 (jangan lapor ulang).
5. Tambahkan uji regresi bila temuan punya repro (ikuti gaya test yang sudah ada di `#[cfg(test)]`).
