# Plan Perbaikan Hang UI Agent — v2 (Ringan tapi Bagus)

> Tujuan: menghentikan UI freeze saat agent streaming konten besar (plan 7000+ char + blok kode),
> tanpa rewrite arsitektur. Semua perubahan frontend; backend tidak disentuh.
> React 19 + Zustand 5 + react-markdown 10 + rehype-highlight 7 (lihat `package.json`).

## Perubahan vs plan v1 (hasil review)

1. **Bug di draft v1 diperbaiki**: buffer `ThoughtDelta` harus di-bind ke **id pesan**, bukan "pesan
   terakhir" — handler `PhaseStarted` (`agent.ts:291-319`) membuat pesan baru dan meng-clone semua
   pesan lama (`agent.ts:297-300`), jadi flush rAF yang telat bisa menempelkan teks ke fase yang salah.
2. **Aturan urutan event ditambahkan**: setiap event non-delta harus flush buffer teks secara sinkron
   dulu, agar teks→tool→teks tetap berurutan.
3. **Klaim investigasi #5 dikoreksi**: `.agent-section` (`global.css:1156-1162`) memakai background
   solid, **bukan** `backdrop-filter`. Blur hanya di `.glass-panel`/modal — beban compositor saat
   streaming lebih kecil dari dugaan. Fase 4 turun jadi opsional murni.
4. Ditambahkan: memoisasi `HistoryMessageView`, selektor Zustand granular, unit test `buildLiveItems`,
   acceptance criteria, dan catatan React StrictMode (dev mode 2x render).

## Akar masalah (sudah diverifikasi ulang di kode)

| # | Penyebab | Lokasi | Impact | Fix |
|---|----------|--------|--------|-----|
| 1 | `rehypeHighlight` menokenisasi ulang SELURUH konten per delta → O(n²) | `AgentPanel.tsx:85-87` | Kritis | Fase 1a |
| 2 | `set()` per chunk teks → ratusan re-render/detik | `agent.ts:323-362` | Tinggi | Fase 1b |
| 3 | `LiveAssistantMessage`/`ToolCallCard` tanpa `React.memo` → pesan lama ikut render | `AgentPanel.tsx:14,55` | Tinggi | Fase 2a |
| 4 | `buildLiveItems` dihitung inline tiap render | `AgentPanel.tsx:342` | Sedang | Fase 2b |
| 5 | `useAgent()` destructuring = subscribe seluruh store | `AgentPanel.tsx:238-244` | Sedang | Fase 2c |
| 6 | Auto-scroll paksa layout tiap delta | `AgentPanel.tsx:255-258` | Sedang | Fase 3a |
| 7 | Output tool penuh (≤4000 char) di DOM tanpa batas | `AgentPanel.tsx:30-32` | Sedang | Fase 3b |
| 8 | `backdrop-filter` `.glass-panel` (history/modal) | `global.css:70-77` | Rendah | Fase 4 (opsional) |

---

## Fase 1 — Hilangkan O(n²) (impact tertinggi)

### 1a. Plain text saat streaming, markdown penuh setelah selesai

`AgentPanel.tsx:83-89` — ganti blok markdown `LiveAssistantMessage`:

```tsx
{msg.content && (
  msg.streaming ? (
    <pre className="agent-streaming-text">{msg.content}</pre>
  ) : (
    <div className="markdown-body">
      <Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
        {msg.content}
      </Markdown>
    </div>
  )
)}
```

`global.css`:
```css
.agent-streaming-text {
  margin: 0; white-space: pre-wrap; word-break: break-word;
  font-family: var(--font-mono); font-size: 12px; line-height: 1.45;
  color: var(--text-primary); user-select: text;
}
```

- Highlight.js kini jalan **sekali per pesan** (saat `streaming=false`), bukan per delta.
- Trade-off UX: ada "flash" teks→markdown saat fase selesai.
- **Alternatif bila flash dianggap mengganggu**: tetap render `<Markdown remarkPlugins={[remarkGfm]}>`
  saat streaming **tanpa** `rehypePlugins`, tambah highlight setelah selesai. Parsing markdown jauh
  lebih murah daripada highlight, tapi tetap O(n) per frame — pakai hanya bila 1a dirasa terlalu
  menurunkan UX. Default: 1a versi `<pre>`.

### 1b. Koalesing `ThoughtDelta` via rAF (dengan id-binding)

Modul-scope di `agent.ts` (satu run aktif pada satu waktu, dijaga flag `busy`):

```ts
let textBuffer: { msgId: string; text: string } | null = null;
let flushRaf: number | null = null;

const flushTextBuffer = () => {
  flushRaf = null;
  const buf = textBuffer;
  textBuffer = null;
  if (!buf?.text) return;
  set((s) => {
    const idx = s.liveMessages.findIndex((m) => m.id === buf.msgId);
    if (idx < 0) return s;
    const messages = [...s.liveMessages];
    messages[idx] = { ...messages[idx], content: messages[idx].content + buf.text };
    return { ...s, liveMessages: messages };
  });
};
```

Di cabang `ThoughtDelta` (`agent.ts:331-332`):

```ts
const target = get().liveMessages;
const lastMsg = target[target.length - 1];
if (lastMsg?.role !== 'assistant') return;
if (textBuffer && textBuffer.msgId !== lastMsg.id) flushTextBuffer(); // ganti fase → flush dulu
if (!textBuffer) textBuffer = { msgId: lastMsg.id, text: kind.ThoughtDelta };
else textBuffer.text += kind.ThoughtDelta;
if (flushRaf === null) flushRaf = requestAnimationFrame(flushTextBuffer);
```

Aturan pesanan wajib — panggil `flushTextBuffer()` **sinkron di awal** handler untuk setiap event
non-`ThoughtDelta` (`ToolCallStarted`, `ToolCallCompleted`, `PhaseStarted`, `PhaseCompleted`,
`Finished`, `Error`), dan juga di `finally` (`agent.ts:386-388`) serta `newChat()` supaya tidak ada
teks yang nyangkut antar-run. Hasil: `set()` teks maksimal ~60fps, urutan event tetap benar, dan
delta tidak pernah salah-sasaran saat fase berganti.

---

## Fase 2 — Memoisasi & selektor

### 2a. `React.memo` dengan compare eksplisit

```tsx
const ToolCallCard: React.FC<{ call: UiToolCall }> = React.memo(({ call }) => { /* ... */ });

const LiveAssistantMessage: React.FC<{ msg: UiMessage }> = React.memo(
  ({ msg }) => { /* ... */ },
  (prev, next) =>
    prev.msg.content === next.msg.content &&
    prev.msg.streaming === next.msg.streaming &&
    prev.msg.toolCalls === next.msg.toolCalls &&
    prev.msg.minimized === next.msg.minimized &&
    prev.msg.phaseSummary === next.msg.phaseSummary &&
    prev.msg.tokensUsed === next.msg.tokensUsed,
);
```

Catatan penting: compare memakai referensi `toolCalls`; store hanya membuat array baru untuk pesan
terakhir yang diubah (`agent.ts:346-350`), jadi pesan lama stabil. `PhaseStarted` memang meng-clone
semua pesan (`agent.ts:297-300`), tapi compare menangkap `minimized`/`streaming` sehingga hanya
terjadi satu re-render sah untuk kolaps.

### 2b. `useMemo` untuk `buildLiveItems`

```tsx
const liveItems = React.useMemo(() => buildLiveItems(liveMessages), [liveMessages]);
```

(Mengganti pemanggilan inline di `AgentPanel.tsx:342`. `liveMessages` tetap berubah tiap flush, jadi
keuntungan utama adalah skip saat re-render yang dipicu state lain.)

### 2c. Selektor granular (ganti destructuring seluruh store)

`AgentPanel.tsx:238-244` saat ini `const { ... } = useAgent()` → re-render tiap `set()` apa pun
(termasuk `sessions`, key-check, dll). Pecah jadi selektor per-field (Zustand 5 pakai `Object.is`):

```tsx
const liveMessages = useAgent((s) => s.liveMessages);
const busy = useAgent((s) => s.busy);
const error = useAgent((s) => s.error);
// ... dst per field yang dipakai
```

Upaya mekanis (~12 field). Dengan 1b, frekuensi sudah dibatasi; selektor ini memastikan panel tidak
reaktif terhadap perubahan store yang tidak berhubungan.

### 2d. Memo `HistoryMessageView`

```tsx
const HistoryMessageView: React.FC<{ msg: ChatMessage }> = React.memo(({ msg }) => { /* ... */ });
```

Array `historyMessages` diganti utuh hanya saat `loadHistory`, jadi referensi stabil dan memo efektif —
load sesi besar tidak ikut re-render saat streaming berjalan.

---

## Fase 3 — Scroll & DOM hemat

### 3a. Auto-scroll hanya jika user di bawah, via rAF

```tsx
const stickToBottom = useRef(true);
const handleScroll = () => {
  const el = scrollRef.current;
  if (el) stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
};
// <div className="agent-stream" ref={scrollRef} onScroll={handleScroll}>

useEffect(() => {
  if (!stickToBottom.current) return;
  const el = scrollRef.current; if (!el) return;
  const id = requestAnimationFrame(() => { el.scrollTop = el.scrollHeight; });
  return () => cancelAnimationFrame(id);
}, [liveMessages, historyMessages]);
```

### 3b. Truncate output tool di DOM (600 char + toggle)

```tsx
const [showFull, setShowFull] = useState(false);
{call.output && (
  <>
    <pre className={`tool-call-output ${call.status === 'error' ? 'error' : ''}`}>
      {showFull ? call.output : truncate(call.output, 600)}
    </pre>
    {call.output.length > 600 && (
      <button className="tool-call-toggle" onClick={() => setShowFull((v) => !v)}>
        {showFull ? 'show less' : `show ${call.output.length - 600} more`}
      </button>
    )}
  </>
)}
```

Opsional kecil: batasi juga `prettyJson(call.argumentsJson)` yang di-expand (argumen
`multi_replace_file` bisa ribuan char) dengan truncate yang sama.

---

## Fase 4 — CSS, hanya jika profil masih berat (opsional)

Koreksi: `.agent-section` (`global.css:1156`) sudah background solid — blur hanya dari
`.glass-panel` (history/error/popup) dan modal overlay. Setelah Fase 1-3, kemungkinan besar tidak
perlu apa-apa. Bila profiler compositor masih menunjukkan blur mahal, turunkan
`--glass-blur: blur(20px) → blur(8px)` (`global.css:40`).

---

## Tes

Repo punya vitest (`npm test`). Tambah unit test ringan untuk logika baru:
- `buildLiveItems`: grouping run berurutan, user bubble memisahkan run, pesan tanpa `runId` jadi run sendiri.
- Buffer koalesing (ekstrak `enqueueDelta`/`flushTextBuffer` jadi helper yang bisa di-import): dua
  delta berturutan jadi satu flush; flush sinkron saat event non-delta; ganti `msgId` memicu flush dulu.

## Verifikasi

1. Tipe & build: `npm run build` (menjalankan `tsc && vite build`).
2. Unit test: `npm test`.
3. Manual: `COPYFILE_DISABLE=1 npm run tauri dev`, jalankan tugas swarm kompleks (plan panjang +
   banyak fase + blok kode). **Catatan: `React.StrictMode` (main.tsx) menggandakan render di dev** —
   bila masih terasa berat di dev, cek juga perilaku production build sebelum menyimpulkan.

## Acceptance criteria

- Selama streaming, mengetik di input dan scroll panel tetap responsif (tidak ada jeda >100ms).
- React DevTools: saat streaming hanya pesan terakhir yang re-render; pesan lama & history tidak.
- Setelah fase selesai, markdown + syntax highlight tetap tampil penuh dan benar.
- Urutan teks/tool di UI identik dengan sebelum fix (tidak ada teks nyelip/tertukar antar fase).

## Risiko & rollback

- **1a**: flash teks→markdown saat fase selesai (cosmetic) → fallback: varian tanpa-highlight di atas.
- **1b**: perubahan urutan event bila ada event baru yang belum di-handle → mitigasi: aturan
  flush-sinkron + unit test; rollback = kembalikan handler lama.
- Semua fase independen → bisa di-ship bertahap; rollback per file.

## Out of scope

- Backend (`orchestrator.rs` truncation sudah benar), skema event/IPC, dependensi baru.
- Virtualisasi daftar pesan (react-window) — hanya relevan jika satu run punya ribuan pesan.

## Urutan pengerjaan

1a → 1b → 2a → 2b → 2c → 2d → 3a → 3b → tes → (Fase 4 hanya jika perlu).
Fase 1a saja biasanya sudah menghilangkan gejala hang utama.
