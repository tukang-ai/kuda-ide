# 🐛 Laporan Bug & Perbaikan — KudaIDE

> Hasil audit mendalam 4 ronde terhadap seluruh codebase (Rust backend `src-tauri/` + React/TS frontend `src/`).
> Semua temuan diverifikasi langsung ke kode; sebagian besar temuan sandbox diverifikasi **empiris** dengan serangan nyata terhadap kernel Python yang diekstrak dari source.
>
> Status akhir: **cargo check ✅ · cargo test 120/120 ✅ · tsc --noEmit 0 error ✅ · vitest 8/8 ✅**

---

## Ringkasan

| Batch | Fokus | Jumlah Bug Diperbaiki |
|-------|-------|----------------------|
| 1 | Keamanan sandbox RLM + jalur tulis file | 9 |
| 2 | Deadlock, race condition, parsing stream, UI state | 11 |
| 3 | Logika swarm fail-open/fail-closed, korupsi data frontend | 13 |
| 4 | Escape sandbox tingkat lanjut, korupsi massal Replace All, kebocoran token, lost-update | 30 |
| **Total** | | **63 diperbaiki · 7 residu terdokumentasi** |

Commit: `97e3707` (batch 1–3) dan `8b0d4d5` (batch 4) pada `main`.

---

## Batch 1 — Keamanan Sandbox & Jalur Tulis

| # | Severity | File | Bug | Perbaikan |
|---|----------|------|-----|-----------|
| 1.1 | 🔴 Kritis | `agent/rlm_kernel.rs` | Closure-leak: kode model mengekstrak `builtins.open` asli via `helper.__globals__['open'].__closure__` → baca/tulis file apa pun | PEP 578 audit hook sekali-per-proses (`_rlm_audit_hook`) yang mencegat event `open`/`open_code` di level CPython — tidak bisa dilewati via introspeksi Python |
| 1.2 | 🔴 Kritis | `agent/rlm_kernel.rs` | `os.posix_spawn`/`posix_spawnp` tidak diblokir → eksekusi proses dari dalam sandbox | Masuk blocklist mutasi `os.*` |
| 1.3 | 🟠 High | `agent/rlm_kernel.rs` | `_io.FileIO` / `_io.open_code` tidak dipatch → baca file apa pun tanpa scope check; `sqlite3`/`dbm`/`shelve` membuka file lewat C code (bisa MENCIPTAKAN file = primitif tulis) | Patch FileIO/open_code dengan scope+denylist (exempt `.py*` untuk import); blok impor sqlite3/dbm/shelve |
| 1.4 | 🟠 High | `commands/project.rs` | Injeksi perintah Windows: URL dikonkatkan ke argumen `start` via shell | Spawn `rundll32 url.dll,FileProtocolHandler` langsung (arg-vector) + filter metacharacter |
| 1.5 | 🟠 High | `commands/fs.rs` | Path approval relatif di-resolve terhadap CWD, bukan project root → approval untuk path di luar proyek bisa lolos | Resolve terhadap project root + canonicalize |
| 1.6 | 🟡 Medium | `agent/rlm_cache.rs` | Fast-path `ctime_ns == 0` terbalik → cache menyajikan konten basi setelah file berubah | Inversi kondisi diperbaiki; fallback sha256 tetap ada |
| 1.7 | 🟡 Medium | `diff_engine/history.rs` | Restore checkpoint memakai nama temp statis → tabrakan `main.rs`/`main.ts`; restore tanpa safety snapshot = kerja setelahnya hilang permanen | Nama temp unik (UUID suffix) + checkpoint "pre-revert" sebelum restore |
| 1.8 | 🟡 Medium | `agent/tool_registry.rs` | `multi_replace_file` fallback global-find mengganti kemunculan PERTAMA tanpa cek ambiguitas | Tolak bila target muncul >1 kali ("refuse to guess") |
| 1.9 | 🟢 Low | `components/SettingsModal.tsx` | `plan_gate_enabled` tidak ikut tersimpan saat Save → setting diam-diam kembali default | Ikut disertakan dalam payload save |

---

## Batch 2 — Race Condition, Deadlock & Stream Parsing

| # | Severity | File | Bug | Perbaikan |
|---|----------|------|-----|-----------|
| 2.1 | 🔴 Kritis | `terminal/pty_manager.rs`, `multiplexer.rs` | Mutex multiplexer dipegang saat membaca output PTY → semua tab terminal beku ketika satu shell flood output | Handle child dibungkus `Arc`, `PtySession` jadi `Clone`; lock dilepas sebelum I/O |
| 2.2 | 🟠 High | `agent/chat_history.rs`, `commands/agent.rs` | Poisoning `SESSION_IO_LOCK` (panic saat hold) membuat SEMUA operasi history gagal selamanya | Helper `session_io_lock()` dengan pemulihan poison (`into_inner`) |
| 2.3 | 🟠 High | `agent/tool_registry.rs` | Single-permit `Notify`: klik cancel ganda membuat run kedua tidak bisa dibatalkan | `CancelFlag::cancelled()` idempoten; 11 titik panggil dimigrasi |
| 2.4 | 🟠 High | `agent/swarm.rs` | Direction gate fail-open: parse gagal → dianggap "lanjut" dan fase eksekusi berjalan tanpa persetujuan | Fail-closed di kedua jalur; minta revisi bounded |
| 2.5 | 🟠 High | `commands/agent.rs` | Verifier tanpa verdict ditampilkan sebagai sukses palsu | Pola "confirmable" — butuh konfirmasi eksplisit pengguna |
| 2.6 | 🟡 Medium | `providers/gemini.rs`, `providers/openai.rs` | Frame SSE non-JSON ditelan diam; `finishReason: SAFETY`/`blockReason` Gemini diabaikan → respons kosong tanpa penjelasan | Frame non-JSON dilaporkan sebagai error; finish/block reason ditangani eksplisit |
| 2.7 | 🟡 Medium | `store/agent.ts` | Listener `hub-auth-success` terdaftar ulang tiap mount → callback ganda | Flag modul-scope `authListenerBound` |
| 2.8 | 🟡 Medium | `agent/rlm_kernel.rs` | Timeout dari config tanpa batas (hang tanpa batas) + akumulasi output tanpa batas sampai timeout | Clamp `(1,120)` detik + capture cap 200K karakter (pipe tetap didrain) |
| 2.9 | 🟡 Medium | `components/SettingsModal.tsx` | Komponen didefinisikan di dalam render → remount tiap keystroke (fokus input hilang) | Dipindah ke module scope (`ModelField`, `RoleRow` dengan prop `ctx`) |
| 2.10 | 🟢 Low | `store/workspace.ts`, `components/AgentPanel.tsx` | Guard dirty menolak reload eksternal tapi tidak menjaga ketikan saat await; IME composition memicu submit prematur | Re-check pasca-await (diperluas di batch 4); guard `isComposing` |
| 2.11 | 🟢 Low | `chat_history.rs`, `commands/agent.rs` | File chat history mode default (bisa dibaca user lain); verifier error menyisipkan raw string ke URL | Mode `0600`; percent-encode |

---

## Batch 3 — Logika Swarm & Integritas Data Frontend

| # | Severity | File | Bug | Perbaikan |
|---|----------|------|-----|-----------|
| 3.1 | 🔴 Kritis | `agent/swarm.rs` | Audit plan `complete:true` fail-open di chat path → gap tak terdeteksi dianggap selesai | Default `complete:false` + entry gap; cache hanya menyimpan hasil valid |
| 3.2 | 🔴 Kritis | `agent/swarm.rs` | Parse plan gagal → **plan dummy "Execute task" difabrikasi lalu DIEKSEKUSI** | Loop re-draft bounded (max 8 ronde) → abort dengan pesan jelas jika tetap gagal |
| 3.3 | 🔴 Kritis | `agent/orchestrator.rs` | Repair truncated-JSON juga dipakai untuk tool MUTASI → `write_file` terpotong direpair menjadi tulisan setengah jalan yang tampak sah | `MUTATING_TOOLS` (`write_file`, `multi_replace_file`, `run_command`) tidak pernah di-repair; self-correcting error |
| 3.4 | 🟠 High | `agent/orchestrator.rs` | Parameter `_emitted_any` diabaikan → restart turn setelah output terlihat = side effect dobel | Restart hanya bila belum ada output |
| 3.5 | 🟠 High | `agent/chat_orchestrator.rs` | Channel UI tertutup (reload webview/crash) → run terus berjalan habis token secara invisible | Send gagal → `cancel_flag.cancel()` |
| 3.6 | 🟠 High | `store/agent.ts` | Tool call yang DITOLAK user direplay berstatus `"done"` → riwayat menyesatkan | Parse `ToolResult.is_error` → status `'error'` |
| 3.7 | 🟡 Medium | `agent/orchestrator.rs` | Error "akan di-retry otomatis" padahal Err langsung mematikan fase (janji palsu) + false-positive fatal | Self-correcting nudge re-emit tag lengkap dalam loop bounded |
| 3.8 | 🟡 Medium | `agent/swarm.rs` | Merge konteks executor salah aritmetika indeks (`skip(len)`/slice meleset) → baseline window log bocor/salah potong | Merge berbasis konten (`PartialEq` pada `Message`/`ToolCallChunk`) |
| 3.9 | 🟡 Medium | `components/TerminalPanel.tsx` | Kill tab aktif → panel kosong padahal shell lain hidup; reaper bocor `bus`; output awal PTY (prompt/MOTD) hilang sebelum subscribe | Neighbor-select; `dropSessionBuffers`; pending-buffer 100K char direplay ke subscriber pertama |
| 3.10 | 🟡 Medium | `components/EditorPane.tsx` | Model Monaco tidak didispose saat unmount (leak snapshot dokumen); `setValue` menghapus undo stack | Dispose semua model saat unmount; `pushEditOperations` sebagai pengganti |
| 3.11 | 🟢 Low | `store/agent.ts` | `deleteSession` aktif meninggalkan transcript & gate menunjuk sesi mati | Clear `liveMessages`/`resumeTarget`/pending gates |
| 3.12 | 🟢 Low | `lib/ipc.ts`, `store/workspace.ts` | Tipe `FsEvent` (`string[]`) ≠ serde Rust (`{Modified: "/path"}`) | Tipe disesuaikan enum externally-tagged |
| 3.13 | 🟢 Low | `App.tsx` | Shortcut Cmd+B/J/I mencuri fokus dari input/Monaco | Skip bila target INPUT/TEXTAREA/contentEditable/Monaco |

---

## Batch 4 — Audit Mendalam (Sandbox Escape, Mass Corruption, Token Leak)

### 4a. KRITIS/HIGH

| # | Severity | File | Bug | Perbaikan |
|---|----------|------|-----|-----------|
| 4.1 | 🔴 Kritis | `agent/rlm_kernel.rs:917` | Spawn python tanpa `-I`: file `base64.py`/`time.py` di root proyek dieksekusi **penuh sebelum guard terpasang** (sys.path[0]=cwd). *Terverifikasi empiris.* | Argumen spawn `-I` (isolated: `-P -E -s`) |
| 4.2 | 🔴 Kritis | `agent/rlm_kernel.rs` | `import _ctypes; _ctypes.dlopen(...)` lolos dari semua guard pure-Python (blocklist hanya `ctypes` wrapper) | Poison `_ctypes`, `_cffi_backend`, `mmap`, `_winapi` |
| 4.3 | 🔴 Kritis | `agent/rlm_kernel.rs` | Audit hook **fail-open**: state kosong → `return` (semua open lolos saat tampering) | Fail-closed: hanya `.py*` bootstrap yang lolos; sisanya raise |
| 4.4 | 🟠 High | `agent/rlm_kernel.rs` | `os.link` tidak diblokir → hardlink `~/.ssh/id_rsa` ke scratch dir (allowlisted) → dibaca utuh. *Terverifikasi empiris.* | Blokir `link/symlink/mkfifo/mknod/mkdtemp` + varian `*at` |
| 4.5 | 🟠 High | `agent/rlm_kernel.rs` | Exempt suffix `.py` dicek pada STRING MENTAH: symlink `evil.py → ~/Documents/notes.txt` dibaca via `io.FileIO`. *Terverifikasi empiris.* | Realpath SEBELUM uji suffix, di 3 titik (FileIO, open_code, audit hook); denylist pada ejaan raw+resolved |
| 4.6 | 🟠 High | `agent/rlm_kernel.rs` | Guard membersihkan `os.environ` tapi bukan `os.environb` (mirror C environ) → secret bocor verbatim. *Terverifikasi empiris.* | `os.environb` dibangun dari dict yang sudah disaring |
| 4.7 | 🔴 Kritis | `indexer/search.rs:133` | **Replace All memperlakukan teks pengganti sbg regex-template**: ganti `cost`→`$100` menghapus SEMUA match (`$1`=group kosong) di seluruh proyek, tanpa peringatan | `replacement_is_literal` default true (insert verbatim via closure); `$1` tetap expand hanya di mode regex eksplisit (semantik VS Code) |
| 4.8 | 🔴 Kritis | `components/SearchPanel.tsx` | Replace All mengabaikan scope/include/exclude (filter hanya klien-side atas tampilan) → semua file di proyek diedit | Backend menerima daftar file eksplisit dari hasil TERFILTER |
| 4.9 | 🟠 High | `agent/key_store.rs:49` | Catch-all env `LLM_API_KEY` dipakai sbg master-token hub → secret tak terkait dikirim sbg `Bearer` ke pihak ketiga tiap chat | Lookup master token keychain-only (`get_api_key_from_keychain`) |
| 4.10 | 🟠 High | `commands/agent.rs:1157`, `hub_session.rs` | `provider_save` menerima base_url sembarang untuk `kuda_hub`; refresh membawa master token PERMANEN ke URL itu → exfiltration | Refresh dipin ke konstanta resmi `HUB_BASE_URL`; save menolak origin asing |

### 4b. MEDIUM

| # | Severity | File | Bug | Perbaikan |
|---|----------|------|-----|-----------|
| 4.11 | 🟡 Med | `agent/tool_registry.rs` | Fallback `multi_replace_file` bisa match DI DALAM teks hasil insert chunk lain → edit salah lokasi yang tampak sukses | Pelacakan `inserted_spans` + shift delta; match overlap ditolak |
| 4.12 | 🟡 Med | `file_system/io.rs`, `store/workspace.ts` | Lost-update dua arah: save user menimpa hasil agent (dan sebaliknya) tanpa cek apapun | Precondition SHA-256 `savedContent` di `fs_write_file` → conflict error; reconcile pasca-save |
| 4.13 | 🟡 Med | `diff_engine/history.rs` | `revert_session` abort mid-loop (`?`) → pohon setengah-reverted tanpa indikasi | Continue-on-error per-file; return `{reverted, failed}`; UI menampilkan kegagalan |
| 4.14 | 🟡 Med | `diff_engine/history.rs` | Metadata checkpoint korup ditelan diam → "Nothing to revert" bohong, .bak orphan | `tracing::warn!` per file korup |
| 4.15 | 🟡 Med | `store/workspace.ts` (watcher/reload) | Dirty-guard dievaluasi SEBELUM await, applied SESUDAHNYA dari snapshot basi → ketikan saat read hilang | Re-check dirty dari `getState()` fresh pasca-await (2 lokasi) |
| 4.16 | 🟡 Med | `components/SettingsModal.tsx` | Save Token manual menulis `token_key` (session id) ke slot MASTER → rotasi rusak, user lockout 30 menit; fallback offline menulis master→session slot | Delegasikan refresh ke backend `agentRefreshHubSession` (keep-master); hapus fallback destruktif |
| 4.17 | 🟡 Med | `tauri.conf.json` | CSP tidak memuat `https://kuda-ide.my.id` → fetch hub produksi selalu gagal → memicu jalur fallback 4.16 | Origin hub ditambahkan; `object-src 'none'`; `base-uri 'self'`; buang `ws://localhost` dev leftover |
| 4.18 | 🟡 Med | `gateway/auth_loopback.rs` | Endpoint pickup balas `ACAO: *` + PNA true → website mana pun di tab lain bisa POST pickup code sendiri (credential swap) | Header CORS/PNA dihapus total (flow asli = navigasi top-level); verifikasi Host loopback; `Cache-Control: no-store` |
| 4.19 | 🟡 Med | `commands/agent.rs:101` | Polling usage memakai MASTER token (non-expiring) alih-alih session key rotasi | Selalu kirim session key; error jelas bila session kosong |
| 4.20 | 🟡 Med | `agent/rlm_kernel.rs` | Memo/snippet-bank poisoning: `_rlm_load.__globals__['_rlm_index'][key] = {...}` menyuntik konten ke context agent | Store enkapsulasi method-only (`_rlm_MemoStore`/`_rlm_BankStore`/`_rlm_Counter`, tanpa `__setitem__`) |
| 4.21 | 🟡 Med | `agent/rlm_kernel.rs` | DoS: `raise SystemExit(0)` mematikan kernel persisten; `dup2`/close stdio merusak protokol sentinel | SystemExit ditelan di boundary exec user-code; dup2-to-stdio & close(0/1/2) diblokir |
| 4.22 | 🟡 Med | `agent/rlm_kernel.rs` | Scratch dir nama statis → TOCTOU swap `cmd_<uuid>.py` oleh user lokal | Dir unik per proses (`pid + uuid`, 0700) via `OnceLock` |

### 4c. LOW (semua diperbaiki)

| # | File | Bug | Perbaikan |
|---|------|-----|-----------|
| 4.23 | `tool_registry.rs` | Splice replacement LF ke file CRLF → EOL campuran dalam satu region | `normalize_eol()` mengikuti gaya mayoritas region |
| 4.24 | `file_system/io.rs` | Ranged read `lines().join("\n")` menghilangkan `\r` + info trailing newline (landmine tulis-balik) | Slicing byte-level mempertahankan terminator asli |
| 4.25 | `io.rs`, `history.rs` | tmp+rename me-reset permissions → script kehilangan exec bit saat di-edit/restore | `atomic_write_preserving_permissions()` menyalin mode bits |
| 4.26 | `io.rs` | TOCTOU new-file: writer lain menciptakan file di celah exists()-check → checkpoint "created" → revert MENGHAPUS file mereka | Re-check pasca-checkpoint; snapshot konten pemenang race |
| 4.27 | `swarm.rs` ×4 | Artefak `.kuda` (plan/brief/handoff) ditulis `fs::write` telanjang → crash = file terpotong | Lewat helper atomik |
| 4.28 | `history.rs`, `project.rs` | History checkpoint tumbuh tanpa batas (tiap Cmd+S = salinan penuh) | `prune_old_sessions(30 hari)` opportunistic di project open |
| 4.29 | `store/agent.ts` | `newChat`/`loadHistory`/`deleteSession` tak ter-guard `busy` → transkrip tercampur antar sesi | Early-return + pesan |
| 4.30 | `store/agent.ts` | Load history merender ledger mentah DAN rekonstruksi → semua prompt tampil 2× + JSON blob | Simpan rekonstruksi saja |
| 4.31 | `store/agent.ts` | Double-click tombol gate/approve → error banner palsu ("No pending…") | Gate/request dibersihkan sebelum invoke |
| 4.32 | `store/workspace.ts` | Double-click file → tab duplikat + React key ganda | Registry in-flight `pendingOpens` + re-check pasca-await |
| 4.33 | `AgentPanel.tsx` | Enter ganda di celah render → prompt kedua lenyap (`setInput('')` sebelum cek busy fresh) | Baca `getState().busy` fresh sebelum clear |
| 4.34 | `store/agent.ts` | Resume sukses hanya clear streaming milik user bubble (map tak pernah match) | Clear streaming semua pesan |
| 4.35 | `store/agent.ts` | Resume dengan sessionId `''` → backend fork sesi baru; tombol resume untuk direct-chat pasti gagal | `resumeTarget` hanya untuk swarm + session diketahui; guard `!sessionId` |
| 4.36 | `lib/liveItems.ts` | Resume run lama → dua group dengan runId sama → React key duplikat | Field `groupKey` unik per grup (runId tetap raw) |
| 4.37 | `store/workspace.ts` | Watcher hidup terus setelah project close; cleanup return value tidak pernah dipakai | Disimpan & dipanggil di `closeProject` |
| 4.38 | `store/agent.ts` | `liveMessages` tumbuh tanpa batas; counter `reloadingFromDisk` tak pernah dibersihkan | Cap 800 pesan; prune counter per applyExternalContent |
| 4.39 | `FileExplorer.tsx` | `useWorkspace()` tanpa selector → re-render seluruh tree per keystroke editor | Narrow selector per field |
| 4.40 | `FileExplorer.tsx` | Rename/delete menulis disk dulu, konfirmasi dirty-tab belakangan → save menghidupkan file usang | Konfirmasi dirty SEBELUM mutasi disk |

---

## ⚠️ Residu Terdokumentasi (Tidak Diperbaiki — Butuh Keputusan/Koordinasi)

| ID | Item | Alasan Ditunda |
|----|----------------------|
| R1 | **Sandbox OS-level** (`sandbox-exec`/Seatbelt atau container/uid terpisah) di sekeliling kernel Python | Rekomendasi struktural #1 dari adversarial review: containment pure-Python terhadap kode hostil tidak akan pernah sempurna. Perubahan arsitektural, butuh keputusan distribusi |
| R2 | `RLIMIT_AS`/`RLIMIT_NPROC` via `pre_exec` (anti memory/thread bomb) | Butuh dependensi `libc` atau deklarasi FFI manual; macOS menangani RLIMIT_AS secara beda |
| R3 | `DirEntry.stat()` pada symlink masih men-stat target di level C (kebocoran metadata existence/size) | Memfilter entries mahal; dampak rendah (tanpa isi file) |
| R4 | Block `import sys` transitif membuat beberapa paket stdlib murni (`multiprocessing`, `runpy`, `pickle`) gagal impor | Ini justru salah satu penghalang kecelakaan menuju spawn; melonggarkan butuh testing luas |
| R5 | PKCE verifier dikirim di **query-string** `/auth/pending?verifier=…` (berpotensi masuk access log Cloudflare) | Kontrak API hub sisi server; butuh dukungan header/body dari server sebelum klien berubah |
| R6 | Sign Out tidak melakukan revoke sisi server (master token tetap valid di hub) | Belum ada endpoint revoke yang diketahui di hub; butuh koordinasi backend |
| R7 | Kredensial hub plaintext di Windows tanpa ACL ketat; `serde_json::to_string_pretty(...).unwrap_or_default()` berpotensi menimpa kredensial dengan file kosong bila serialize gagal | Hardening Windows-specific + perbaikan satu-baris `unwrap_or_default()` belum dieksekusi di batch ini |

> Catatan R7 bagian kedua (`unwrap_or_default`) adalah perbaikan satu baris yang layak di-pick cepat.

---

## Metodologi Verifikasi

1. **Setiap temuan diverifikasi manual** ke kode sebelum masuk daftar (tidak ada laporan agent yang diterima mentah-mentah).
2. **Serangan sandbox diuji empiris**: raw-string Python diekstrak dari `rlm_kernel.rs` via regex, stub Rust-side dibangun, lalu serangan nyata dijalankan (ctypes, hardlink, symlink-environb, environb). Contoh: `PASS - B5 FileIO symlink`, `BLOCKED` untuk `_ctypes`.
3. **Regressi**: 38 test sandbox RLM + 120 test lib Rust + 8 test vitest frontend dijalankan hijau setelah setiap kelompok perubahan; `tsc --noEmit` bersih.
4. Serangan yang terbukti **GAGAL** menembus guard (coverage positif): subprocess/os.system semua varian, thread-based escape, socket TCP & AF_UNIX langsung, write-mode opens, importlib.reload, marshal bytecode, sentinel spoofing, output flooding.

---

*Dibuat otomatis dari sesi audit — commit referensi: `97e3707`, `8b0d4d5`.*
