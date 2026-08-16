# Plan Multi-Turn Swarm — "Thinker Ledger" + Plan Approval Gate

> Tujuan: turn N+1 tahu pemikiran turn N (plan, brief, hasil edit + verdict) tanpa membocorkan
> transcript executor. Hemat token via append-only ledger per turn + prefix cache. Display layer
> penuh (semua fase) dikirim dari backend, bukan localStorage.
> Lingkup: `agent_swarm_chat` (swarm). `agent_chat` (direct) sudah persist full trace — tidak diubah.
>
> **Status implementasi**: Batch 1-3 SUDAH diimplementasikan dan lulus build + test
> (backend `cargo test` 79/79, frontend `npm run build` + `vitest` 8/8).
> Perubahan penting dari desain: eksekusi HANYA terjadi saat user mengklik "Eksekusi"
> (lihat §14.3 / §14.6) dan window konteks epoch aktif lewat `build_ledger_context`.
>
> **Status v2**: hasil review via simulasi persona "user proyek kompleks, multi-turn lintas hari,
> butuh pengawasan, AI sering salah paham maksud" (§13) + integrasi fitur **Plan Approval Gate**:
> setelah Thinker bikin plan, run berhenti menunggu user — Edit plan / Minta Reviewer / Eksekusi
> (§14). Ditambah **Ledger Epoch Compaction** untuk sesi panjang (§15).

---

## 0. Prinsip keras (dari konsep, dipertahankan)

1. **Append-only, urutan tetap.** Satu pesan assistant per turn = satu blok ledger. Blok lampau
   tidak pernah ditulis ulang. Pelanggaran = prefix cache miss untuk seluruh historis → mahal.
2. **Pemisahan konteks vs display.** `session.messages` = ledger (untuk AI context, hemat).
   `session.transcript` = `Vec<PhaseRecord>` (untuk replay UI, lengkap). Keduanya saling
   melengkapi; transcript TIDAK pernah masuk konteks LLM.
3. **Executor privat, review publik.** Transcript tool executor (grep/rlm_python/edit loop) tidak
   masuk ledger maupun transcript yang masuk konteks. Yang masuk ledger = rangkuman diff per-file
   + verdict + issues. Yang masuk transcript (display) = teks fase + tool call ringkas (tanpa
   raw output besar) — display saja, bukan konteks.
4. **Fail-open + backward compat.** Sesi lama ([user, final_answer]) tetap load; ledger kosong
   diisi separuh. Tidak ada migrasi data.
5. **Hemat token per turn.** Setiap segmen ledger dipotong (brief ~1500, plan ~2000,
   exec_review ~2000, final_answer ~1500). Total ledger ≤ ~7.000 char/turn.
6. **Human-in-the-loop di boundary termahal.** Gate persetujuan plan setelah Thinker (§14):
   plan yang salah paham tidak boleh langsung dieksekusi — eksekusi adalah biaya terbesar
   (executor + fix round + checkpoint revert). Menunggu keputusan user = **0 token**.
7. **Keputusan & koreksi user = ground truth intent.** Status approval/revisi (`[PLAN STATUS]`)
   direkam di ledger dan TIDAK BOLEH hilang saat kompresi, supaya turn berikutnya tidak
   menebak-nebak ulang maksud user. Koreksi plan ditangani lewat gate (saran → Edit plan → Thinker).

---

## 1. Audit kondisi saat ini (akar masalah, verifikasi kode)

| # | Kondisi | Lokasi | Akibat |
|---|---------|--------|--------|
| 1 | `agent_swarm_chat` hanya append `user_message` + satu `final_message` | `agent.rs:265`, `agent.rs:320-334` | turn N+1 lihat prompt + jawaban akhir turn N saja |
| 2 | `context_messages = session.messages.clone()` + user baru | `agent.rs:279-280` | session file = satu-satunya sumber konteks lintas turn |
| 3 | `SwarmOutcome` bawa `final_answer`, `plan`, `verdict`, `tokens_used` saja | `swarm.rs:151-156` | brief digest & executor report hilang saat `run_swarm` return (mereka hanya ada di `shared` lokal) |
| 4 | brief digest, plan, exec report, verdict hanya di-push ke `shared` (lokal) | `swarm.rs:583-587`, `swarm.rs:812-815`, `swarm.rs:906`, `swarm.rs:1012` | tidak bertahan lintas turn |
| 5 | RLM cache per-PROYEK (bukan per-sesi) + classifier Sufficiency/Incremental/Fresh | `rlm_cache.rs:166-178`, `rlm_cache.rs:478-524` | setelah turn N edit file → manifest berubah → Incremental/Fresh → re-research meski brief turn N sudah ada di percakapan |
| 6 | Display replay pakai `localStorage` `kuda_transcript:{id}` | `agent.ts:84-106`, `agent.ts:220-231`, `agent.ts:430` | tidak cross-machine, bukan source of truth, hilang saat clear cache |

**Inti**: turn N+1 "setengah buta" karena konteksnya hanya {prompt, final_answer}. Plan, brief,
file apa diedit, verdict — semua lenyap.

---

## 2. Struktur data baru (backend, gagasan terkonkret)

### 2a. `TurnLedger` (swarm.rs, di samping `SwarmOutcome`)

```rust
pub struct TurnLedger {
    pub brief_digest: Option<String>,     // validated RLM brief digest (turn ini)
    pub plan_markdown: Option<String>,   // final plan (versi yang di-eksekusi/disetujui)
    pub plan_status: Option<String>,      // BARU v2: "approved (review×1, revision×2)" |
                                          // "auto (gate off)" | "CANCELLED AT GATE" | None
    pub execution_review: Option<String>,// diff per-file ringkas + verdict + top issues
    pub final_answer: String,             // jawaban akhir Thinker
}
```

- Direndaer jadi **satu** `Message` assistant (role `Assistant`, `name: Some("ledger")`) lalu
  di-append ke session. Menggantikan append `final_message` tunggal (`agent.rs:334`).
- Untuk jalur awal (direct answer / plan gagal / exhausted turns / no doc) isi separuh:
  `plan_markdown = None`, `execution_review = None`, `brief_digest` = apa pun yang terproduksi,
  `final_answer` = `synth.final_text`. Tetap satu append per turn.

### 2b. `SwarmOutcome` diperluas (atau bawa `TurnLedger`)

```rust
pub struct SwarmOutcome {
    pub final_answer: String,
    pub plan: Option<SwarmPlan>,
    pub verdict: Option<Verdict>,
    pub tokens_used: usize,
    pub ledger: TurnLedger,            // BARU
    pub transcript: Vec<PhaseRecord>, // BARU (display, bukan konteks)
}
```

`final_answer`/`plan`/`verdict` tetap (tidak break caller). `ledger` & `transcript` baru.

### 2c. `PhaseRecord` + `PhaseToolCall` (chat_history.rs, ikut `ChatSessionData`)

```rust
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PhaseToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
    pub output: String,        // sudah di-truncate di backend (≤600 char) sebelum simpan
    pub status: String,        // "running" | "done" | "error"
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PhaseRecord {
    pub run_id: String,        // kelompokkan fase satu run
    pub role: String,          // AgentRoleKey
    pub label: String,         // PhaseStarted.label
    pub model: String,
    pub summary: String,       // PhaseCompleted.summary
    pub text: String,          // akumulasi ThoughtDelta fase ini (= outcome.final_text)
    pub tool_calls: Vec<PhaseToolCall>,
    pub created_at: DateTime<Local>,
}
```

```rust
pub struct ChatSessionData {
    pub meta: ChatSessionMeta,
    pub messages: Vec<Message>,        // = ledger (konteks AI)
    #[serde(default)]
    pub transcript: Vec<PhaseRecord>,  // = display replay (BARU, fail-open default kosong)
    pub checkpoint_ids: Vec<String>,
}
```

`#[serde(default)]` → sesi lama load dengan transcript kosong (fallback display ke messages).

### 2d. `TranscriptCollector` (swarm.rs, helper internal)

Membangun `Vec<PhaseRecord>` dengan **tee** dari event stream — sumber kebenaran tunggal,
logika sama dengan handler frontend (`agent.ts:339-365`, `agent.ts:382-418`) tapi di Rust.

```rust
struct TranscriptCollector {
    run_id: String,
    records: Vec<PhaseRecord>,
    current: Option<PhaseRecordBuilder>, // akumulasi text + tool_calls fase aktif
}
impl TranscriptCollector {
    fn record(&mut self, kind: &AgentEventKind);  // ThoughtDelta/ToolCall*/PhaseStarted/PhaseCompleted/Finished/Error
    fn finish(self) -> Vec<PhaseRecord>;
}
```

- `emit(kind)` di `run_swarm` (`swarm.rs:194-196`) diubah: panggil `on_event.send(...)` (UI live)
  **dan** `collector.record(&kind)` (persist). Tidak ada duplikasi logika fase.
- `PhaseRecordBuilder` menampung `text` (append `ThoughtDelta`), `tool_calls` (push saat
  `ToolCallStarted`, update saat `ToolCallCompleted`), `summary` (saat `PhaseCompleted`).
- `output` tool di-truncate ≤600 char saat disimpan (sudah ada pola di `AgentPanel.tsx:30-32` /
  `PERFORMANS_FIX_PLAN.md` Fase 3b — sekarang di backend).
- `ExternalAccessRequest`/`Resolved` di-skip di v1 (approval interaktif, tidak perlu replay).

---

## 3. Format blok ledger (dirender ke satu pesan assistant)

Prompt user TIDAK diulang di ledger (sudah ada sebagai `Message` user terpisah, `agent.rs:265`).
Mengulang = duplikasi token. Urutan konteks turn N+1:

```
[user: prompt_1][assistant: ledger_1][user: prompt_2][assistant: ledger_2]...[user: prompt_{N+1}]
```

Isi satu pesan ledger (urutan tetap, setiap segmen di-truncate):

```
[RESEARCH BRIEF] <brief_digest, ~1500 char>        ← RLM turn N+1 baca brief turn N
[PLAN] <plan_markdown, ~2000 char>                 ← Thinker turn N+1 tahu plan sebelumnya
[PLAN STATUS] approved (review×1, revision×2)      ← v2: intent user terekam (ground truth)
[EXECUTION REVIEW] <file list + verdict + issues, ~2000 char>
                                                   ← Thinker tahu apa diedit & terverifikasi
[FINAL ANSWER] <final_answer, ~1500 char>          ← jawaban akhir turn ini
```

Segmen kosong (jalur awal) ditulis `—` (satu tanda) supaya struktur tetap konsisten & cache-
stabil (panjang tetap mirip, tidak ada segmen yang hilang bergantian).

### 3a. `build_ledger_message(ledger: &TurnLedger, ts: &str) -> Message`

Helper di `swarm.rs` (di samping `render_plan_markdown`, `swarm.rs:1689`). Mengembalikan
`Message { role: Assistant, content: <blok di atas>, name: Some("ledger"), .. }`. Semua segmen
lewat `truncate_chars` (`swarm.rs:2024`) dengan konstanta budget.

### 3b. `build_execution_review_ledger(...)` (kondensasi, BUKAN report 6000 char)

Report `shared` (`[EXECUTOR REPORT]`, `swarm.rs:906`) cap-nya `MAX_REPORT_CHARS=6000`. Untuk
ledger, bangun bentuk ringkas dari akumulasi executor report + verdict:

```
[EXEC] Task #1 (code) by Executor Code — src/foo.rs: +12 -3
[EXEC] Task #2 (design) by Executor Design — static/style.css: +40 -0
[VERDICT] PASSED — all tasks verified.
```

Bila FAILED/UNVERIFIED, sertakan ≤3 top issues (masing-masing ≤120 char). Total cap ~2000 char.
Sumber data: akumulasi `Vec<(task_id, kind, role, file_diff_oneliner)>` + `verdict_opt`.

---

## 4. Matriks visibilitas (turn N+1) — refined dengan referensi kode

| Aktor | Bisa lihat | Tidak boleh lihat |
|-------|-----------|-------------------|
| **RLM Model** | `rlm_ctx = messages.to_vec()` (ledger SEMUA turn) + cache proyek + prompt baru | transcript executor, raw tool output |
| **Thinker** | `shared` (ledger turn lampau + brief + plan turn ini) + prompt baru | transcript executor, eksplorasi RLM/Reviewer |
| **Reviewer** | `shared` (ledger lampau + plan Thinker) | transcript executor |
| **Executor** | `shared` + task brief + tool ctx | transcript executor lain (sudah privat, `swarm.rs:890`) |
| **Executor Reviewer** | `verify_context = shared + executor_logs` (`swarm.rs:923-931`) | — |
| **Thinker final** | `shared` (exec reports + verdict turn ini, `swarm.rs:1012,1066`) | raw executor keystroke |
| **UI (display)** | `session.transcript` (semua fase + tool call ringkas) | — (display, bukan konteks) |
| **User (gate, v2)** | plan draft/revisi markdown penuh via `PlanDecisionRequest` event (§14.2) | tidak perlu lihat transcript executor mentah untuk memutus plan |

Catatan: `rlm_ctx` (`swarm.rs:207`) dan `shared` (`swarm.rs:202`) sama-sama berakar pada
`messages.to_vec()` = `session.messages` (ledger). Maka begitu ledger terimplementasi, RLM &
Thinker OTOMATIS baca pemikiran turn lampau — tanpa perubahan wiring.

---

## 5. Alur data per-turn (urutan implementasi logis)

```
agent_swarm_chat:
  1. load/create session (agent.rs:251-254)
  2. append user_message (agent.rs:265)            ← pesan user terpisah, TETAP
  3. context_messages = session.messages + user_message (agent.rs:279-280)
  4. run_swarm(context_messages, ...) -> SwarmOutcome { ledger, transcript, ... }
  5. ledger_msg = build_ledger_message(&outcome.ledger, ts)
  6. history_mgr.append_message(session_id, ledger_msg, None)   ← GANTI final_message
  7. history_mgr.append_transcript(session_id, &outcome.transcript)  ← BARU
  8. return AgentRunResult { ... }
```

`run_swarm` internal:
```
  - shared = messages.to_vec(); rlm_ctx = messages.to_vec()  (swarm.rs:202,207)
  - collector = TranscriptCollector::new(run_id)
  - emit(kind) = { on_event.send(kind); collector.record(&kind) }   ← tee
  - Phase 0 RLM: brief_text akumulasi -> ledger.brief_digest = truncate(brief_text, 1500)
  - Phase 1 Thinker: draft_plan; bila early-exit -> ledger separuh + return (lihat §6)
  - Phase 2 Reviewer: final_plan -> ledger.plan_markdown = truncate(render_plan_markdown, 2000)
  - Phase 3-4 Executor+ER: akumulasi exec oneliner + verdict ->
        ledger.execution_review = build_execution_review_ledger(...)
  - Phase 5 Thinker final: final_outcome.final_text -> ledger.final_answer = truncate(...,1500)
  - return SwarmOutcome { ..., ledger, transcript: collector.finish() }
```

---

## 6. Edge cases & early-exit (semua jalur return SwarmOutcome)

Lokasi early-exit saat ini: `swarm.rs:665`, `swarm.rs:695`, `swarm.rs:725`, `swarm.rs:740`
(direct-answer / plan unparseable / exhausted / no doc). Setiap return harus membawa
`TurnLedger` separuh + `transcript`:

| Jalur | brief_digest | plan_markdown | execution_review | final_answer |
|-------|--------------|---------------|-------------------|-------------|
| Direct answer (Thinker jawab tanpa plan) | ada jika RLM jalan | None | None | `thinker_outcome.final_text` |
| Plan unparseable / no doc → synthesize | ada | None | None | `synth.final_text` |
| Thinker exhausted → synthesize | ada | None | None | `synth.final_text` |
| Pipeline penuh (plan→exec→verdict) | ada | `render_plan_markdown` | `build_execution_review_ledger` | `final_outcome.final_text` |
| **v2** Gate: user approve setalah revision×n / review×m | ada | versi FINAL plan + `plan_status="approved (review×m, revision×n)"` | `build_execution_review_ledger` | `final_outcome.final_text` |
| **v2** Gate: user cancel saat menunggu keputusan | ada | draft plan + `plan_status="CANCELLED AT GATE"` | None | "(dibatalkan user pada tahap approval plan)" |
| Run gagal (Err) di `agent.rs:295-315` | — | — | — | append "Run failed: {e}" (TETAP, tidak ada ledger) |

Fail path (`agent.rs:297-314`) tetap append pesan error (tidak ada ledger — turn gagal tidak
perlu konteks pemikiran). `transcript` untuk fail path: collector.finish() berisi fase yang
sudah jalan sebelum gagal → tetap di-append supaya history replay jujur menampilkan apa yang
terjadi sebelum crash.

`final_answer` blank (swarm selesai tanpa output) → isi "Run selesai tanpa jawaban final..."
(pola `agent.rs:320-323` dipertahankan, sekarang jadi segmen `[FINAL ANSWER]`).

---

## 7. Perubahan backend (detail per file, tanpa kode)

### `swarm.rs`
- Tambah `TurnLedger`, `PhaseRecord`, `PhaseToolCall`, `TranscriptCollector`.
- Ubah `emit` (`swarm.rs:194-196`) jadi tee (channel + collector).
- Akumulasi `brief_text` (`swarm.rs:208,568`) → `ledger.brief_digest`.
- Akumulasi executor oneliner di loop exec (`swarm.rs:844-911`) → `ledger.execution_review`.
- Setelah verdict (`swarm.rs:1012`) sertakan verdict di exec review.
- Setelah Phase 5 (`swarm.rs:1100-1105`) isi `ledger.final_answer` + `plan_markdown`.
- Semua 5 return `Ok(SwarmOutcome{...})` (`swarm.rs:665,695,725,740,1100`) isi `ledger` + `transcript`.
- Tambah `build_ledger_message`, `build_execution_review_ledger`.
- Konstanta budget: `LEDGER_BRIEF_CHARS=1500`, `LEDGER_PLAN_CHARS=2000`,
  `LEDGER_EXEC_CHARS=2000`, `LEDGER_ANSWER_CHARS=1500`, `PHASE_TOOL_OUTPUT_CHARS=600`.

### `agent.rs` (`agent_swarm_chat`, `agent.rs:232-340`)
- Ganti blok `final_message` (`agent.rs:320-334`) dengan `build_ledger_message(&outcome.ledger, ts)`
  lalu `append_message`. Tetap **satu** append per turn.
- Tambah `history_mgr.append_transcript(&session_id, &outcome.transcript)` setelah append ledger.
- Fail path (`agent.rs:297-314`) tetap, Tambah `append_transcript(collector.finish())` bila
  `run_result` Err membawa transcript (perlu `SwarmOutcome` Err carry transcript — lihat §8).

### `chat_history.rs`
- Tambah field `transcript: Vec<PhaseRecord>` (`#[serde(default)]`) di `ChatSessionData` (`chat_history.rs:19-23`).
- Tambah `append_transcript(&self, session_id, &Vec<PhaseRecord>) -> Result<()>`: load session,
  extend `transcript`, update `meta.updated_at`, save. Append-only (tidak rewrite history).
- `append_message` (`chat_history.rs:102-130`) tidak berubah (tetap satu pesan per append).

### `rlm_cache.rs` (opsional, synergy) — lihat §9
- Tidak wajib diubah untuk ledger. Classifier `classify_cache_state` (`rlm_cache.rs:478-524`)
  tetap per-proyek. Synergy session-level dihandel via prompt (§9), bukan cache.

### `prompt_composer.rs`
- Tambah satu kalimat di prompt RLM Model: "Prior research briefs from this conversation are
  included above as `[RESEARCH BRIEF]` ledger entries. Before re-researching, judge whether a
  prior brief already covers this request." — membuat RLM reuse brief turn lampau secara eksplisit.
- **Batas peran keras (HARD ROLE BOUNDARY)**: RLM Model = peneliti data SAJA — dilarang memberi
  saran/plan/endorse; brief kosong bila riset tidak perlu. RLM Verifier = audit DATA saja —
  dilarang membuat/menilai plan (pada tahap ini plan belum ada). Thinker = satu-satunya perancang,
  dan diminta mengabaikan bahasa planing di brief bila RLM nyasar. Reviewer (plan) tetap berhak
  merevisi plan — itu tugasnya.

### `roles.rs` / `orchestrator.rs`
- Tidak berubah. `AgentEventKind` (`orchestrator.rs:68-87`) & `RoleLoopOutcome` (`orchestrator.rs:417-433`)
  cukup. TranscriptCollector konsumsi `AgentEventKind` (sudah ada).

---

## 8. Perubahan frontend (revert localStorage + pakai transcript backend)

### `types.ts`
- Tambah `PhaseRecord`, `PhaseToolCall` (mirror backend, `types.ts:107-125` region).
- Tambah `transcript?: PhaseRecord[]` di `ChatSessionData` (`types.ts:78-82`).

### `store/agent.ts`
- **Hapus** `transcriptKey`, `saveTranscript`, `loadTranscript`, `clearTranscript`
  (`agent.ts:84-107`) dan pemanggilannya (`agent.ts:242`, `agent.ts:430`).
- `loadHistory` (`agent.ts:220-231`): `liveMessages = mapTranscript(data.transcript)`;
  bila `transcript` kosong (sesi lama) fallback ke `mapMessages(data.messages)`.
  `historyMessages` tidak lagi dipakai untuk replay swarm (hanya direct chat bila perlu).
- `mapTranscript(PhaseRecord[]): UiMessage[]`: satu `UiMessage` per `PhaseRecord`
  (role assistant, content = `rec.text`, `agentRole = rec.role`, `phaseLabel/Model/Summary`,
  `toolCalls`, `runId`). Grouping `runId` (`AgentPanel.tsx buildLiveItems`) tetap jalan.
- `send` (`agent.ts:419-444`): hapus `saveTranscript(...)`. Transcript kini disimpan backend
  via `append_transcript`; frontend cukup refresh `sessions` list.

### `AgentPanel.tsx` / `ipc.ts`
- `ipc.agentSwarmChat` return tetap `AgentRunResult` (chat_session_id, edit_session_id).
  Frontend tidak butuh transcript hasil invoke (UI live sudah streaming via channel);
  transcript di-fetch ulang saat `loadHistory`.
- `ipc.chatLoadSession` otomatis dapat `transcript` (backend tambah field). Tidak ada invoke baru.
- Render `PhaseRecord` → `UiMessage` tidak butuh komponen baru (struktur `UiMessage` sama,
  `agent.ts:15-31`); `AgentPanel` sudah render `UiMessage` per fase.

---

## 9. Synergy: RLM reuse brief turn lampau (hemat)

Setelah ledger terimplementasi, `rlm_ctx` (`swarm.rs:207`) sudah berisi ledger turn lampau
(yang menyimpan `[RESEARCH BRIEF]` turn sebelumnya). RLM Model turn N+1 OTOMATIS lihat brief N.

- **Tanpa prompt change**: model sudah bisa melihat brief lampau di konteks.
- **Dengan prompt change** (`prompt_composer.rs`, §7): eksplisit disuruh menilai sufficiency
  brief lampau sebelum re-research. Mengurangi ronde eksplorasi RLM saat follow-up ringan.
- **Cache proyek** (`rlm_cache.rs`) tetap pegang perananya (manifest diff → Sufficiency/
  Incremental/Fresh). Ledger = lapisan SESSION; cache = lapisan PROJECT. Tumpang tindih aman:
  bila cache Fresh tapi brief lampau di ledger cukup, RLM Model bisa submit brief lampau
  (Sufficiency tingkat sesi) tanpa re-collect.

Tidak ada perubahan `classify_cache_state` — keputusan reuse tetap di tangan RLM Model lewat
konteks ledger, bukan classifier.

---

## 10. Hemat token — invarian & anggaran

### Prefix cache (input murah)
- Satu append per turn, blok lampau immutable. Provider prefix cache mengenali prefix
  `[ledger_1..N]` identik → input turn N+1 murah (hanya prompt baru + turn terakhir full).
- **Aturan keras**: tidak ada rewrite/mutation pesan lampau. Edit data = tambah blok baru.
  `append_message` & `append_transcript` hanya push, tidak truncate/rewrite.

### Anggaran per-turn (konstanta di `swarm.rs`)
| Segmen | Char | Sumber |
|--------|------|-------|
| `[RESEARCH BRIEF]` | 1500 | `brief_text` (`swarm.rs:568`) |
| `[PLAN]` | 2000 | `render_plan_markdown` (`swarm.rs:1689`) |
| `[EXECUTION REVIEW]` | 2000 | `build_execution_review_ledger` (kondensasi) |
| `[FINAL ANSWER]` | 1500 | `final_outcome.final_text` (`swarm.rs:1101`) |
| Total ledger | ~7000 | satu pesan assistant/turn |

Transcript (display, BUKAN konteks) tidak masuk anggaran token LLM; cap disk per tool output
600 char, fase text = `final_text` (tidak dibatasi — display kebutuhan pengguna).

### Lain
- Executor transcript tetap privat (`executor_logs`, `swarm.rs:842,890`) — tidak bocor.
- `truncate_chars` (`swarm.rs:2024`) dipakai semua segmen (middle-truncation konsisten).
- Direct chat (`agent_chat`) tidak diubah (sudah append full trace, `agent.rs:181-183`).

---

## 11. Testing

### Backend (`src-tauri/tests/` atau `#[cfg(test)]`)
- `build_ledger_message`: segmen kosong → `—`; semua segmen ter-truncate sesuai budget; urutan
  tetap `[RESEARCH BRIEF][PLAN][EXECUTION REVIEW][FINAL ANSWER]`.
- `build_execution_review_ledger`: PASSED/FAILED/UNVERIFIED; >3 issues di-cap; file list oneliner.
- `TranscriptCollector::record`: ThoughtDelta append; ToolCallStarted→Completed update status;
  PhaseStarted finalize fase lama + buat fase baru; PhaseCompleted set summary; Finished finalize.
- `TurnLedger` separuh di setiap early-exit (`swarm.rs:665,695,725,740`).
- `chat_history.append_transcript`: append (tidak rewrite); load roundtrip; sesi lama (tanpa
  field) load `transcript == []`.
- Integrasi: jalankan `run_swarm` dengan mock provider → SwarmOutcome.ledger & transcript terisi.
- **v2 gate**: registry oneshot resolve("revise") → Thinker revisi + gate kedua; resolve("review")
  → reviewer jalan; resolve("execute") → lanjut Phase 3; counter `plan_status` benar
  ("approved (review×1, revision×2)"); cap 8 ronde memaksa execute/batal; cancel saat await →
  ledger `CANCELLED AT GATE`.
- **v2 kompaksi**: `compact_epoch` prioritas (plan_status + file + verdict; brief/plan lama
  terbuang pertama); window konteks = epochs + ledger epoch berjalan.

### Frontend (`vitest`)
- `mapTranscript`: `PhaseRecord[]` → `UiMessage[]` benar (role/label/runId grouping).
- `loadHistory` fallback: `transcript` kosong → pakai `messages`.
- **v2**: render card `PlanDecisionRequest` (tombol 3 opsi + catatan); `user_gate` PhaseRecord
  tampil di replay; toggle `plan_gate_enabled` persist.

### Manual
- `COPYFILE_DISABLE=1 npm run tauri dev`. Prompt swarm A → edit file. Prompt B "ubah yang tadi"
  → Thinker tahu file + verdict turn A (cek konteks via log/atau jawaban menyebut file lama).
- Tutup app, buka sesi sama → history replay tampil semua fase (dari backend transcript, bukan
  localStorage). Clear localStorage → tetap replay (buktikan tidak depend localStorage).
- Sesi lama (pre-ledger) load tanpa crash (transcript kosong → fallback messages).

---

## 12. Urutan pengerjaan & risiko

### Urutan (independen per lapis, bisa ship bertahap)
1. **Backend data**: `TurnLedger`, `PhaseRecord`, `PhaseToolCall` struct + serde (`swarm.rs`,
   `chat_history.rs`). Build pass.
2. **TranscriptCollector** + tee `emit` → `run_swarm` return transcript. Test collector.
3. **Ledger builder** (`build_ledger_message`, `build_execution_review_ledger`) + isi `ledger`
   di semua 5 return path `run_swarm`.
4. `agent.rs`: ganti `final_message` → `append_message(ledger_msg)` + `append_transcript`.
5. `prompt_composer.rs`: kalimat sufficiency brief lampau (§9).
6. **Frontend**: hapus localStorage transcript, `mapTranscript`, `loadHistory` pakai backend.
7. Test + manual verify.

### Risiko & rollback
- **Risiko 1**: tee `emit` mengubah path event → UI live. Mitigasi: `on_event.send` dipanggil
  pertama, collector kedua (non-blocking). Rollback: kembalikan `emit` lama (collector off).
- **Risiko 2**: ledger besar membebani konteks pada sesi panjang. Mitigasi: anggaran per segmen
  + angka 7000 char/turn; bila perlu, tambah kompresi ringan (hapus segmen brief pada turn >3).
  Rollback: turunkan budget atau skip segmen plan/exec untuk turn lama.
- **Risiko 3**: `append_transcript` memperbesar file sesi. Mitigasi: transcript display-only,
  cap per tool output 600 char; bila file membengkak, lazy-load transcript terpisah (v2).
- **Risiko 4**: sesi lama tanpa `transcript` field. Mitigasi: `#[serde(default)]` → aman.
- Semua langkah independen → rollback per file.

### Out of scope
- `agent_chat` (direct) — sudah append full trace.
- Virtualisasi daftar fase (react-window) — hanya jika satu run ribuan fase.
- Lazy-load transcript terpisah dari file sesi (optimasi v2 bila file membengkak).
- Menyimpan reasoning_content/chain-of-thought ke ledger (privasi + token) — tidak masuk.

---
---

# BAGIAN V2 — HASIL REVIEW & FITUR PENGAWASAN

## 13. Review via simulasi: persona proyek kompleks, multi-turn, butuh pengawasan

**Persona**: mengerjakan proyek besar dengan swarm ini, puluhan turn lintas hari. Keluhan utama:
AI sering salah memahami maksud; sekali salah paham bisa langsung merusak kode karena pipeline
swarm berjalan RLM → Thinker → Reviewer → Executor → ER → Final **tanpa berhenti sama sekali**.

### Yang dinilai sudah benar di v1
1. Append-only ledger + prefix cache — fondasi biaya untuk sesi panjang sudah tepat.
2. Pemisahan konteks (ledger, hemat) vs display (transcript, lengkap) — bersih.
3. Executor privat / review publik — verifikasi tetap terlihat tanpa bloat konteks.
4. Backward compat via `#[serde(default)]` — sesi lama aman tanpa migrasi.

### Gap yang ditemukan dari simulasi

| # | Gap | Dampak bagi persona | Solusi |
|---|-----|---------------------|--------|
| GAP-1 | **Tidak ada gate manusia**; pipeline tidak pernah berhenti | Plan yang salah paham langsung dieksekusi = token executor terbuang + file rusak + harus restore checkpoint | §14 Plan Approval Gate |
| GAP-2 | Ledger tumbuh linear tanpa batas; sesi 20 turn ≈ 140K char (~35-40K token) prefix tiap request | Menabrak batas context window; prefix cache menurunkan harga tapi **tidak** memperkecil ukuran window | §15 Epoch Compaction |
| GAP-3 | Koreksi user ("bukan itu maksud saya") hanya pesan user biasa | Turn berikutnya bisa mengulang salah paham yang sama | §16 (koreksi via gate: saran → Edit plan → Thinker) |
| GAP-4 | Keputusan persetujuan user tidak tercatat di mana pun | Thinker turn berikutnya second-guess / mengubah apa yang sudah disetujui | §14.4 (`plan_status`) |
| GAP-5 | Cancel saat menunggu keputusan user belum terdefinisi | Draft plan lenyap, run menggantung | §14.6 |
| GAP-6 | Reviewer otomatis selalu jalan walau plan mungkin langsung dieksekusi / direvisi user | Pemborosan saat user sudah tahu apa yang ia mau | §14 (reviewer jadi user-initiated) |

Kesimpulan review: **fondasi ledger v1 tetap dipakai apa adanya**; yang harus ditambah adalah
lapisan pengawasan (gate), lapisan umur konteks (kompaksi epoch), dan lapisan koreksi intent.

---

## 14. Plan Approval Gate (human-in-the-loop) — desain detail

### 14.1 Konsep
Setelah Thinker men-submit plan, run **berhenti dan menunggu keputusan user**. Opsi:

1. **Edit plan** — user menulis catatan revisi → Thinker merevisi plan → kembali ke gate.
2. **Minta Reviewer** — slot Reviewer terkonfigurasi mengkritik & boleh merevisi (alur sudah ada,
   `swarm.rs:782-808`) → plan hasil review → kembali ke gate. ("Reviewer lain" = slot reviewer
   berikutnya; multi-slot sudah didukung `resolve_role_providers`, `swarm.rs:760`.)
3. **Eksekusi** — lanjut Phase 3 (executor → Executor Reviewer → final answer) tanpa gate lagi.

Reviewer otomatis bawaan pipeline berubah menjadi **user-initiated** (reviewer hanya jalan bila
user memilih opsi 2). Toggle `plan_gate_enabled` — **default ON**, checkbox di AgentPanel sejajar
toggle auto-approve, persist di `AgentConfig` (`agent_config_get/set`, `agent.rs:520-537`).
Gate OFF = perilaku pipeline lama persis (`plan_status = "auto"`).

### 14.2 Mekanisme pause: reuse pola `ExternalAccessRequest`
Pola tunggu-keputusan-user sudah ada di codebase (approval akses eksternal: tool
`request_external_access` di `tool_registry.rs`, event `orchestrator.rs:79-86`, command
approve/deny `agent.rs:369-393`, registry oneshot di `state.external_requests`). Gate memakai
pola yang sama:

- **Event baru** di `AgentEventKind` (`orchestrator.rs:68-87`):
  - `PlanDecisionRequest { request_id, plan_markdown, round, tasks_count, latest_note: Option<String> }`
  - `PlanDecisionResolved { request_id, decision, note }`
- **Command baru** di `agent.rs`:
  `agent_resolve_plan_decision(state, request_id, decision, note)` dengan
  `decision ∈ {"execute" | "revise" | "review"}`.
- **Registry baru** di `state.rs`: `plan_decisions` (mirror `ExternalRequestRegistry`: oneshot
  channel per `request_id`; menunggu tanpa batas waktu — `agent_cancel_run` tetap berfungsi).
- Di `run_swarm`: emit `PlanDecisionRequest` → `await` oneshot → jalankan cabang keputusan.

### 14.3 State machine di dalam `run_swarm`

```
RLM phase (brief)
    │
Thinker → draft plan                        (swarm.rs:641-747, jalur plan valid)
    │
┌───▼ GATE ─────────────────────────────────────────────┐
│ emit PlanDecisionRequest(plan markdown, round)        │◄──────────┐
│ await keputusan user  (0 token)                       │           │
│   ├─ "review"  → Reviewer run (swarm.rs:782-808) ──► plan revisi ─┤
│   ├─ "revise"  → Thinker + catatan user ──────────► plan revisi ─┤
│   └─ "execute" → keluar gate                                       
└───────────────────────────────────────────────────────┘
    │
Phase 3-5 (executor → ER → fix round → final answer)   (swarm.rs:835-1106)
```

Aturan loop gate:
- **Hanya plan TERAKHIR + catatan revisi terbaru** yang masuk `shared`; versi antara plan TIDAK
  masuk konteks (hemat token) — riwayat revisi penuh hanya di transcript (display).
- **Eksekusi HANYA terjadi bila user mengklik "Eksekusi".** Decision tak dikenal / `revise`/`review`
  setelah cap TIDAK pernah otomatis masuk mode execute — gate kembali menunggu keputusan eksplisit.
- Setelah "Minta Reviewer", user TETAP bisa mengajukan edit (kembali ke gate → "Edit plan" →
  Thinker memperbaiki plan hasil review). Reviewer merevisi plan menjadi SATU versi final yang
  menggantikan versi lama di konteks (`shared.truncate` + push `[REVIEWER PLAN]`) — tidak ada
  versi ganda/kotor.
- Cap `MAX_PLAN_GATE_ROUNDS = 8` per turn; setelah cap, tombol revise/review di UI dinonaktifkan
  (`round >= 8`), hanya "Eksekusi" / "Batal" yang tersisa — tetap bukan eksekusi otomatis.
- Cancel (`agent_cancel_run`) saat menunggu = §14.6.
- `SwarmOutcome` tidak berubah bentuk — gate sepenuhnya internal `run_swarm`; transcript
  collector (§2d) otomatis menangkap event gate.

### 14.4 Rekam keputusan di ledger & transcript
- `TurnLedger.plan_status = Some("approved (review×1, revision×2)")` — dihitung dari counter
  loop gate; `"auto"` bila gate OFF; `"CANCELLED AT GATE"` bila §14.6.
- Blok ledger menambah satu baris `[PLAN STATUS] ...` tepat setelah segmen `[PLAN]` (§3).
- Transcript: event gate menjadi `PhaseRecord` pseudo-role `user_gate` (label = opsi yang
  dipilih + catatan revisi), sehingga replay history memperlihatkan KENAPA plan jadi seperti itu.

### 14.5 Mengapa ini menjawab "AI sering salah paham"
- **Salah paham berhenti SEBELUM eksekusi** — biaya kegagalan turun dari "executor + fix round +
  restore checkpoint" menjadi "1 plan Thinker (+1 reviewer opsional)".
- Catatan revisi user masuk konteks Thinker → koreksi eksplisit, bukan tebakan.
- `plan_status` di turn berikutnya menjadi anchor intent: Thinker baru tahu "plan turn lalu
  disetujui user setelah 2 revisi" — tidak asal mengubah apa yang sudah disetujui.
- Gate juga titik pengawasan natural: user bisa membaca plan sebelum satu byte pun file tersentuh.

### 14.6 Edge case gate
| Kasus | Perilaku |
|-------|----------|
| Cancel saat menunggu | Run berakhir jujur: append ledger dengan draft plan + `plan_status="CANCELLED AT GATE"`, `execution_review=None`, final_answer="(dibatalkan user pada tahap approval plan)". Append-only konsisten; turn berikutnya tahu ada plan tertinggal. |
| User belum klik eksekusi | Run TIDAK pernah masuk mode execute. Decision tak dikenal → kembali ke gate; `revise`/`review` setelah cap 8 ronde → kembali ke gate (hanya Eksekusi/Batal). Eksekusi = satu-satunya jalan ke Phase 3+. |
| Review lalu user mau edit | Reviewer menghasilkan SATU plan revisi → kembali ke gate → user klik "Edit plan" + catatan → Thinker memperbaiki plan hasil review → kembali ke gate. |
| App restart saat gate terbuka | Run in-memory hilang. v1: draft plan sudah terekam di transcript (event di-collector saat emit) → user tinggal kirim ulang prompt. v2 (opsional): persist `pending_plan.json` per session + command `agent_swarm_resume_gate`. |
| Plan 0 task di-"eksekusi" | Langsung ke jalur final answer tanpa plan (jalur existing), `plan_status="approved (no tasks)"`. |
| Gate OFF | Pipeline lama utuh (reviewer otomatis); `plan_status="auto"`. |
| Multi-reviewer slot | Opsi "review" menjalankan semua slot reviewer berurutan seperti alur lama (`swarm.rs:760-829`), lalu kembali ke gate sekali. |

### 14.7 Frontend gate (ringkas)
- `store/agent.ts`: state `pendingPlanDecision` (mirror pola `pendingExternalRequests`) diisi
  dari event `PlanDecisionRequest`; aksi `resolvePlanDecision(requestId, decision, note)` →
  invoke `agent_resolve_plan_decision`.
- `AgentPanel.tsx`: card keputusan di tengah stream — render `plan_markdown` (markdown penuh,
  collapsible per task), field catatan, tombol **Edit plan** / **Minta Reviewer** / **Eksekusi**.
  Card tetap di history replay (dari PhaseRecord `user_gate`).
- `types.ts`: tambah dua varian `AgentEvent` (`PlanDecisionRequest`/`PlanDecisionResolved`).

---

## 15. Ledger Epoch Compaction (sesi panjang, menutup GAP-2)

**Masalah**: turn ke-20 → prefix ≈ 20 × 7000 char ≈ 140K char (~35-40K token). Prefix cache
membuatnya murah secara harga, tetapi tetap bisa menabrak **batas ukuran context window**.

**Desain** (tetap append-only & cache-stabil):
- Bagi historis menjadi epoch berukuran `EPOCH_SIZE = 10` turn (keputusan user: jangan kehilangan
  konteks — 10 turn penuh dipertahankan sebelum ringkasan; jangan diubah jadi lebih kecil tanpa
  kesepakatan).
- Saat turn ke-`10i` di-append, **hitung sekali** blok `[EPOCH SUMMARY i]` (~500 char): goal tiap
  turn dalam epoch, file yang disentuh, verdict, hasil 1-baris. Ringkasan immutable — tidak
  pernah dihitung ulang.
- Konteks turn N = `[EPOCH 1][EPOCH 2]…[EPOCH k]` + ledger turn-turn epoch berjalan + prompt baru.
  Ledger 10 turn penuh (≈70K char) + ringkasan epoch lama tetap masuk → konteks tidak terpotong
  terlalu dini; pertukaran: prefix lebih besar daripada epoch-5, namun 10 turn terakhir selalu
  tersedia utuh untuk rujukan lintas-turn.
- Cache miss hanya terjadi **sekali per 10 turn** (saat summary baru masuk), lalu stabil lagi.
  Bandingkan tanpa kompresi: prefix terus membesar setiap turn sampai mentok window.

**Prioritas isi ringkasan** (bila melewati budget 500 char/epoch):
1. `plan_status` + file yang diedit + verdict (keadaan dunia/keputusan) — **tidak boleh dibuang**.
2. Goal + hasil 1-baris per turn.
3. Detail brief/plan lama — dibuang pertama (turn baru bisa re-research via rlm_cache bila perlu).

**Lokasi impl**: `compact_epoch(...)` di `chat_history.rs`/`swarm.rs`, dipanggil
`agent_swarm_chat` setelah append ledger; summary disimpan sebagai pesan `name: "epoch"`.
Pemilihan konteks `[epochs + ledger epoch berjalan]` terjadi saat menyusun `context_messages`
(`agent.rs:279`) — **file sesi tetap append-only murni** di disk; yang berubah hanya window
yang dikirim ke LLM.

---

## 16. Koreksi intent (DIPUTUSKAN: lewat gate, bukan prefix)

Mekanisme `[USER OVERRIDE]` (checkbox + prefix + aturan prompt + prioritas kompaksi) **DIHAPUS
pada implementasi** setelah review user: koreksi plan bukan urusan prefix pesan, melainkan
bagian alur gate.

- **Cara koreksi yang benar**: di gate, user menulis saran ke kotak "Saran untuk Thinker" lalu
  klik **Edit plan** → Thinker merevisi plan → gate tampil lagi dengan plan terbaru
  (ditulis ulang ke `.kuda/plan.md`). Satu loop sampai plan benar, lalu klik **Eksekusi**.
- Koreksi antar-turn (prompt baru di turn berikutnya) berjalan normal: pesan user terakhir
  di konteks + aturan ledger (`[TURN LEDGER]` + `[PLAN STATUS]`) memberi Thinker anchor
  intent, tanpa marker khusus.

---

## 17. Update urutan pengerjaan & risiko (v2)

### Urutan (batch)
- **Batch 1 (fondasi ledger — dari §12)**: step 1-7 sebelumnya. Ledger + transcript backend
  berfungsi penuh tanpa gate; perilaku pipeline tidak berubah.
- **Batch 2 (gate)**: event + registry + command `agent_resolve_plan_decision` → state machine
  gate di `run_swarm` → `plan_status` di ledger → UI card keputusan + toggle `plan_gate_enabled`.
- **Batch 3 (tahan lama)**: Epoch Compaction. Bisa setelah fitur terasa
  kebutuhan nyata (persona baru kena limit di >15 turn), desainnya tidak mengubah Batch 1-2.
  `EPOCH_SIZE = 10` (konteks 10 turn penuh dipertahankan sebelum ringkasan).

### Risiko tambahan (gate)
- **Risiko G1**: user meninggalkan gate berjam-jam → memori run tertahan. Mitigasi: memori per
  run kecil (shared context sudah ada); batas praktis = satu run aktif per sesi. Cancel selalu
  tersedia. v2 resume (§14.6) bila sering terjadi.
- **Risiko G2**: user terbiasa klik "Eksekusi" buta (gate jadi formalitas). Mitigasi: ini masalah
  UX, bukan bug desain — card menampilkan diff-plan bila hasil revisi, jadi klik pun tetap
  memperlihatkan perubahan.
- **Risiko G3**: revisi loop membuat Thinker bingung (plan berubah-ubah). Mitigasi: hanya plan
  terakhir di konteks + catatan revisi terbaru; cap 8 ronde.
- **Risiko G4**: event gate tidak kompatibel dengan replay lama. Mitigasi: pseudo-role
  `user_gate` hanya PhaseRecord tambahan; renderer yang belum tahu menampilkannya sebagai fase
  biasa (fail-open).

### Out of scope v2 (dicatat, bukan dilupakan)
- Gate tambahan SEBELUM eksekutor per-task ("pause sebelum tugas N") — mekanisme registry sama,
  tinggal ditambah; tidak masuk v2 agar eksekusi tetap gesit; checkpoint + revert sudah ada
  sebagai jaring pengaman.
- `agent_swarm_resume_gate` (survive restart) — desain ada (§14.6), implementasi v2.1.
- Deteksi otomatis "user sedang mengoreksi" (NLP) — rapuh & mahal; checkbox eksplisit dipilih.
