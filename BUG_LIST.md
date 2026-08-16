# Daftar Error yang Ditemukan & Akan Diperbaiki

Proyek: `/Users/macmini/.mounty/SSD_External/fix/kuda-ide`
Tanggal: 2026-08-16
Status: dalam perbaikan (centang diupdate setelah selesai)

> Supersedes §4 laporan AGENT_AUDIT_REPORT.md. Bug 1-15 dari sesi pertama,
> 16-30 dari sesi kedua. Semuanya BELUM diperbaiki saat file ini dibuat.

---

## Kritis (backend)

- [ ] **#1 Sandbox RLM Python bypass total via `_orig_*` globals + `importlib.reload(os)`**
  `rlm_kernel.rs`: `READONLY_GUARD_PY` mendefinisikan `_orig_open`/`_orig_os_open`/
  `_orig_fdopen`/`_orig_chdir`/`_orig_scandir`/`_orig_walk`/`_orig_listdir` di
  top-level script yang di-`exec` ke `globals()` yang SAMA dengan kode user.
  Model bisa `_orig_open('<app_data>/hub_credentials.json').read()` (master token)
  atau `importlib.reload(os)` untuk mengembalikan `os.remove/system` (write/delete
  di luar proyek). Seluruh guard read-only jadi tipis.
  Fix: kurung guard dalam closure/function lokal; hapus `_orig_*` dari globals;
  blok `importlib.reload`.

- [ ] **#16 GatewayHub security pipeline dead code — chat bypass seluruh gate**
  `gateway/gateway_hub.rs:54` `process_request` (validasi JWT, device fingerprint,
  intent guard, rate limiter, secure vault, audit log) tidak pernah dipanggil dari
  path chat. `agent_chat`/`agent_swarm_chat` → provider langsung. Rate-limit harian,
  device binding, intent guard tidak berlaku.
  Fix: wire `process_request` di alur chat (token issued di command, stream lewat
  gateway).

## Tinggi (backend)

- [ ] **#2 Hardcoded JWT secret** — `state.rs:41` `"kuda_ide_local_jwt_secret_key_2026"`
  dipakai `GatewayHub::new`. Secret statis di repo.
  Fix: generate random per-app (CSPRNG), persist 0600, fallback random per-process.

- [ ] **#3 `cancel_all()` pada registry app-wide → race antar run**
  `commands/agent.rs:159,317-319` memanggil `external_requests.cancel_all()` /
  `plan_decisions.cancel_all()` / `direction_decisions.cancel_all()` di akhir tiap
  run. Registry singleton di `AppState` → dua run konkuren saling menjatuhkan
  pending request. Fix: hapus panggilan `cancel_all` lintas-run; tag/purge per run.

- [ ] **#4 Direction gate fail-open vs plan gate fail-closed** — `swarm.rs:1240-1244`
  `Err(_) => ("lanjut", None)` (auto-approve), sedangkan plan gate `:1686` →
  `("cancelled", None)`. Fix: jadikan fail-closed konsisten.

- [ ] **#5 `request_external_access` tidak validasi path absolut** — `tool_registry.rs:1437`
  `PathBuf::from(&path)` langsung; path relatif di-canonicalize terhadap CWD.
  Fix: tolak path non-absolut.

- [ ] **#9 Swarm `emit` abaikan channel closed → run jalan terus** — `swarm.rs:478-483`
  `let _ = on_event.send(...)`; beda `run_loop` yang cancel saat UI drop.
  Fix: cancel bila send gagal.

- [ ] **#13 Gemini 401 tidak memicu refresh hub** — `gemini.rs:68` pakai `AppError::General`,
  bukan `AppError::Api { status }`. Fix: parse status → Api.

- [ ] **#19 `grep_search` baca dotfile & secret** — `indexer/search.rs:38` `hidden(false)` →
  `.env`, kredensial masuk konteks agen. Fix: `hidden(true)`.

- [ ] **#20 `fs.rs` approval tanpa broad-root guard** — `commands/fs.rs:30-41`
  satu klik Allow → akses `/`, `~`, `/etc`. Fix: tolak broad root (isi ulang
  guard `is_broad_root`).

## Menengah (backend)

- [ ] **#6 Leak entry `pending` + resolve stale** — `tool_registry.rs:113-133,1414-1451`
  entry tidak dihapus saat timeout/drop. Fix: `remove()` + cleanup di tool.

- [ ] **#7 `execute_code` timeout di internal kernel-call tidak kill kernel** —
  `rlm_kernel.rs`: `sync_kernel_allowlist`/`prewarm`/`inventory_snapshot` timeout
  → proses Python tetap jalan → kernel kotor. Fix: kill on timeout.

- [ ] **#8 Cancel tidak hentikan `run_command`/`rlm_python` mid-eksekusi** —
  `tool_registry.rs:870-908` & `rlm_kernel.rs` haпуşa `tokio::time::timeout` tanpa
  `select!` vs `cancel.notified()`. Fix: gabungkan cancel ke `select!`; perbaiki
  `CancelFlag::cancel()` agar simpan permit (race §4 #1).

- [ ] **#10 Resume token accounting salah** — `swarm.rs:488` `total_tokens_in`
  di-init dari `resume.total_tokens` (in+out). Fix: simpan `tokens_in` di checkpoint.

- [ ] **#11 `get_or_spawn` bandingkan root tekstual** — `rlm_kernel.rs:955` tanpa
  canonicalize → respawn tiap panggilan utk root symlink. Fix: canonicalize compare.

- [ ] **#12 OpenAI tool buffer tidak di-flush jika stream selesai tanpa ``** —
  `openai.rs:101-109,177-179`. Fix: flush di akhir stream (chained once).

- [ ] **#14 `rlm_cache` file cache tanpa mode 0600** — `rlm_cache.rs:302-307`.
  Fix: `OpenOptions::mode(0o600)` + sync + rename.

- [ ] **#15 `build_manifest` membaca dotfile** — `rlm_cache.rs:319` `hidden(false)`.
  Fix: `hidden(true)`.

- [ ] **#17 `secure_vault` lookup key salah** — `secure_vault.rs:47` pakai `provider.id()`
  (nama model) bukan `provider_key.<id>`. Fix: `provider_key.<id>` (+ hub fallback).

- [ ] **#18 `history.rs` `sha256::digest` BUKAN sha256** — `diff_engine/history.rs:356-368`
  `DefaultHasher` (SipHash-64) dipakai sbg fingerprint integritas + bucket dir.
  Fix: pakai `sha2` asli.

- [ ] **#22 Penulisan sesi/config non-atomik** — `chat_history.rs:129` dan
  `provider_config.rs:162` `fs::write` langsung. Fix: tmp+rename.

- [ ] **#23 Race read-modify-write chat session** — `append_message`/`append_transcript`
  load→mutasi→save tanpa lock → data loss saat dua op bersamaan. Fix: mutex per-manager
  (atau per-session) saat save.

- [ ] **#29 `session_id`/`run_id` tanpa validasi di pintu IPC** — `commands/agent.rs`
  terima mentah; andalkan validasi di `load_session`/`checkpoint_path`. Fix: validasi
  di muka dengan `is_safe_id`.

## Rendah (backend + frontend)

- [ ] **#21 `prompt_composer` `collect_symbols` baca file dot** — `:554-589` skip
  hanya direktori bertitik, bukan file dot. Fix: skip file dimulai `.`.

- [ ] **#24 `agent_approve_external_access` abaikan `_scope`** — `commands/agent.rs:547`.
  Fix: hapus param atau validasi/terapkan.

- [ ] **#25 Gemini tool-call duplikat tak bisa dipasangkan** — `gemini.rs:104-120`
  id sintetis `gemini_call_<n>` + response dipasangkan via `name` saja. Fix:
  pasangkan via nama+index konsisten.

- [ ] **#26 `INDEX_CACHE` key non-kanonik + tak pernah evict** — `prompt_composer.rs:437`.
  Fix: canonicalize key + batasi ukuran cache.

- [ ] **#27 `AstParser` hanya simbol top-level** — `indexer/ast.rs:70` satu level;
  method/nested/class/enum/variable/module tak pernah dihasilkan. Fix: walk tree
  penuh & petakan semua `SymbolKind`.

- [ ] **#30 `agent_chat` context tidak di-window** — `commands/agent.rs:132` kirim
  history penuh. Fix: gunakan `build_ledger_context` + `compact_epoch`.

---

## Selesai (akan di-checklist setelah build & test)

- [x] #1 sandbox RLM (function-scoped guard, blok reload/import)
- [x] #2 JWT secret random per-process
- [x] #3 cancel_all lintas-run dihapus + remove() per-request
- [x] #4 direction gate fail-closed
- [x] #5 path absolut wajib pada request_external_access
- [x] #6 pending entry dihapus saat timeout/drop
- [x] #7 execute_code kill on timeout
- [x] #8 run_command & rlm_python cancel-aware (+ CancelFlag permit)
- [x] #9 swarm emit cancel saat channel UI closed
- [x] #10 checkpoint simpan tokens_in
- [x] #11 get_or_spawn bandingkan canonical
- [x] #12 openai flush tool buffer di akhir stream
- [x] #13 gemini -> AppError::Api (401/429/5xx)
- [x] #14 rlm_cache write 0600 + sync
- [x] #15 rlm_cache manifest hidden(true)
- [x] #16 GatewayHub di-wire (roles wrap_gateway + process_request)
- [x] #17 secure_vault delegasi ke provider (key baked)
- [x] #18 history.rs sha256 asli (sha2)
- [x] #19 grep_search hidden(true)
- [x] #20 fs.rs broad-root guard
- [x] #21 collect_symbols skip dotfile
- [x] #22 save session/config atomik
- [x] #23 lock RMW chat session
- [x] #24 _scope dihapus
- [x] #25 gemini pairing (documented limitation, no code change)
- [x] #26 INDEX_CACHE canonical key + eviction
- [x] #27 ast.rs full-tree walk (semua SymbolKind)
- [x] #29 validasi session_id/run_id di IPC
- [x] #30 bound_direct_chat_window
- [x] Verifikasi: cargo check ✓, cargo test --lib ✓ (104 pass), tsc ✓, vitest ✓ (8 pass)
- [x] Review tambahan (ditemukan & diperbaiki):
  - Sandbox: `posix`/`_posixsubprocess`/`gc`/`inspect`/`sys` ikut diblokir (posix = C module di balik os → `posix.open/unlink` bypass semua patch os.*; sys = jalur ke `sys.modules['__main__']` yang memuat globals penuh).
  - Bypass gate via mutasi global `_rlm_allowlist` ditutup: `execute_user_code` menjalankan kode model di namespace RESTRICTED (hanya helper RLM + builtins patched), jadi `_rlm_allowlist`/`_rlm_project_root`/`sys` tak terjangkau. Regression test: `test_rlm_user_code_cannot_mutate_allowlist`.
  - Leak stale pending entry saat cancel di `RequestExternalAccessTool` ditutup (drain semua request id setelah loop).