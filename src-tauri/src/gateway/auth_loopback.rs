//! Loopback HTTP server untuk OAuth handoff anti-clipboard (RFC 8252).
//!
//! Pattern standar desktop OAuth (dipakai VS Code, GitHub Desktop): saat login
//! dimulai, IDE spin-up server sementara di `127.0.0.1:PORT`. Browser dinavigasi
//! secara top-level ke `http://127.0.0.1:PORT/pickup?pk=...` (atau POST /pickup).
//! IDE terima request → ekstrak pickup_secret → emit event `auth:pickup` → poll
//! hub untuk token → login selesai.
//!
//! Navigasi top-level ini kebal terhadap aturan Mixed Content Safari/WebKit
//! dan pembatasan clipboard macOS. Bind hanya ke 127.0.0.1 (loopback).

use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// State loopback server: handle spawn + shutdown trigger + slot pickup terbaru.
pub struct LoopbackServer {
    pub port: u16,
    join: Option<JoinHandle<()>>,
    shutdown: Arc<Mutex<bool>>,
    /// Slot pickup code yang dikirim browser (diisi saat GET/POST `/pickup` diterima).
    pub pickup: Arc<Mutex<Option<String>>>,
}

impl LoopbackServer {
    /// Spawn loopback server di `127.0.0.1:0` (port acak bebas). Kembalikan
    /// handle yang berisi port + mekanisme shutdown + slot pickup.
    pub async fn spawn(app: AppHandle) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let shutdown = Arc::new(Mutex::new(false));
        let pickup: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let shutdown_clone = shutdown.clone();
        let pickup_clone = pickup.clone();
        let app_clone = app.clone();

        let join = tokio::spawn(async move {
            loop {
                // Cek shutdown sebelum accept (cepat, tidak block lama).
                if *shutdown_clone.lock().await {
                    break;
                }
                // Accept dengan deadline kecil agar bisa cek shutdown periodik.
                let accept = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    listener.accept(),
                )
                .await;
                let (mut stream, _peer) = match accept {
                    Ok(Ok(c)) => c,
                    Ok(Err(_)) => continue,
                    Err(_) => continue, // timeout → cek shutdown lagi
                };

                // Baca request HTTP.
                let mut buf = vec![0u8; 4096];
                let n = match stream.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();

                // DNS-rebinding / drive-by defense: this endpoint hands out
                // account credentials, so only requests addressed to the
                // loopback interface itself are served.
                let host_ok = raw
                    .lines()
                    .find_map(|l| {
                        let v = l.strip_prefix("Host:")?.trim();
                        Some(
                            v.starts_with("127.0.0.1")
                                || v.starts_with("localhost")
                                || v.starts_with("[::1]"),
                        )
                    })
                    .unwrap_or(false);

                let (status, content_type, body_out) = if !host_ok {
                    (
                        "403 Forbidden",
                        "text/html; charset=utf-8",
                        render_error_html("Forbidden."),
                    )
                } else if raw.starts_with("OPTIONS") {
                    // Preflight: respond EMPTY. No ACAO/ACAPN headers on
                    // purpose — cross-origin JavaScript must NOT be able to
                    // read or post here (`ACAO: *` used to let any website in
                    // any tab inject its own pickup code mid-login). The real
                    // flow is a top-level browser NAVIGATION to /pickup, which
                    // needs no CORS at all.
                    ("204 No Content", "text/plain; charset=utf-8", String::new())
                } else if raw.starts_with("GET /favicon.ico") {
                    // Browser icon request → abaikan tanpa error.
                    ("204 No Content", "text/plain; charset=utf-8", String::new())
                } else if raw.starts_with("GET /pickup") {
                    // Top-level navigation dari browser: GET /pickup?pk=pk_... atau ?pickup=pk_...
                    let first_line = raw.lines().next().unwrap_or("");
                    let path_and_query = first_line
                        .strip_prefix("GET ")
                        .and_then(|s| s.split_whitespace().next())
                        .unwrap_or("");
                    let query = path_and_query.split('?').nth(1).unwrap_or("");
                    let parsed = parse_pickup_from_query(query);

                    if let Some(pk) = parsed {
                        *pickup_clone.lock().await = Some(pk.clone());
                        let _ = app_clone.emit("auth:pickup", pk);
                        ("200 OK", "text/html; charset=utf-8", render_success_html().to_string())
                    } else {
                        (
                            "400 Bad Request",
                            "text/html; charset=utf-8",
                            render_error_html("Kode pickup_secret tidak ditemukan atau tidak valid."),
                        )
                    }
                } else if raw.starts_with("POST /pickup") {
                    // POST /pickup dari background fetch atau form submit.
                    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
                    let parsed = parse_pickup_from_body(body);

                    if let Some(pk) = parsed {
                        *pickup_clone.lock().await = Some(pk.clone());
                        let _ = app_clone.emit("auth:pickup", pk);
                        ("200 OK", "text/html; charset=utf-8", render_success_html().to_string())
                    } else {
                        (
                            "400 Bad Request",
                            "text/html; charset=utf-8",
                            render_error_html("Format body POST pickup tidak valid."),
                        )
                    }
                } else {
                    ("404 Not Found", "text/html; charset=utf-8", render_error_html("Endpoint tidak ditemukan."))
                };

                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                    len = body_out.len(),
                    body = body_out
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        Ok(Self {
            port,
            join: Some(join),
            shutdown,
            pickup,
        })
    }

    /// Ambil pickup code yang sudah diterima (None kalau belum).
    pub async fn take_pickup(&self) -> Option<String> {
        self.pickup.lock().await.take()
    }

    /// Shutdown loopback server (dipanggil setelah login selesai / gagal).
    pub async fn shutdown(&mut self) {
        *self.shutdown.lock().await = true;
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

/// Ekstrak nilai pickup secret dari query string (e.g. `pk=pk_...` atau `pickup=pk_...`).
pub fn parse_pickup_from_query(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next()?.trim();
        let val = parts.next().unwrap_or("").trim();
        if key.eq_ignore_ascii_case("pk") || key.eq_ignore_ascii_case("pickup") {
            let decoded = url_decode_simple(val);
            if let Some(valid_pk) = extract_valid_pk(&decoded) {
                return Some(valid_pk);
            }
        }
    }
    None
}

/// Ekstrak nilai pickup secret dari body (mendukung JSON `{"pickup":"..."}` / `{"pk":"..."}` atau form `pk=...`).
pub fn parse_pickup_from_body(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 1. Coba parse sebagai JSON bila diawali '{'
    if trimmed.starts_with('{') {
        for key in &["\"pickup\"", "\"pk\""] {
            if let Some(idx) = trimmed.find(key) {
                let after = &trimmed[idx + key.len()..];
                if let Some(q1) = after.find('"') {
                    let rest = &after[q1 + 1..];
                    if let Some(q2) = rest.find('"') {
                        let candidate = &rest[..q2];
                        if let Some(valid_pk) = extract_valid_pk(candidate) {
                            return Some(valid_pk);
                        }
                    }
                }
            }
        }
    }

    // 2. Fallback parse form urlencoded
    parse_pickup_from_query(trimmed)
}

/// Validasi dan sanitasi string pickup secret (`pk_...`, panjang minimal 16 karakter).
fn extract_valid_pk(candidate: &str) -> Option<String> {
    let clean = candidate.trim().trim_matches('"').trim_matches('\'');
    if clean.starts_with("pk_")
        && clean.len() >= 16
        && clean.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Some(clean.to_string())
    } else {
        None
    }
}

/// URL decoder sederhana untuk query string tanpa dependency eksternal berat.
fn url_decode_simple(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_val) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                result.push(hex_val as char);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            result.push(' ');
            i += 1;
            continue;
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Render halaman HTML sukses dengan estetika premium KudaIDE (Dark theme).
fn render_success_html() -> &'static str {
    r#"<!DOCTYPE html>
<html lang="id">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Login Berhasil — KudaIDE</title>
    <style>
        * { box-sizing: border-box; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Oxygen, Ubuntu, Cantarell, sans-serif;
            background: #0f172a;
            color: #f8fafc;
            display: flex;
            align-items: center;
            justify-content: center;
            min-height: 100vh;
            margin: 0;
            padding: 20px;
        }
        .card {
            background: #1e293b;
            border: 1px solid #334155;
            border-radius: 16px;
            padding: 40px 32px;
            max-width: 460px;
            width: 100%;
            text-align: center;
            box-shadow: 0 20px 40px rgba(0, 0, 0, 0.6), 0 0 30px rgba(56, 189, 248, 0.1);
            animation: fadeIn 0.3s ease-out;
        }
        @keyframes fadeIn {
            from { opacity: 0; transform: translateY(12px); }
            to { opacity: 1; transform: translateY(0); }
        }
        .check-badge {
            width: 64px;
            height: 64px;
            border-radius: 50%;
            background: rgba(5, 150, 105, 0.18);
            border: 2px solid #059669;
            color: #34d399;
            font-size: 32px;
            line-height: 60px;
            margin: 0 auto 20px;
            box-shadow: 0 0 20px rgba(52, 211, 153, 0.25);
        }
        .badge {
            display: inline-block;
            background: rgba(56, 189, 248, 0.12);
            border: 1px solid rgba(56, 189, 248, 0.3);
            color: #38bdf8;
            padding: 4px 14px;
            border-radius: 20px;
            font-size: 12px;
            font-weight: 600;
            letter-spacing: 0.5px;
            text-transform: uppercase;
            margin-bottom: 12px;
        }
        h2 {
            color: #f8fafc;
            font-size: 24px;
            margin: 0 0 10px;
            font-weight: 700;
        }
        p {
            color: #94a3b8;
            font-size: 14px;
            line-height: 1.6;
            margin: 0 0 24px;
        }
        .highlight {
            color: #38bdf8;
            font-weight: 600;
        }
        .hint-box {
            background: #0f172a;
            border: 1px dashed #334155;
            border-radius: 8px;
            padding: 12px 16px;
            font-size: 12px;
            color: #64748b;
        }
    </style>
</head>
<body>
    <div class="card">
        <div class="check-badge">✓</div>
        <div class="badge">Otentikasi Berhasil</div>
        <h2>Terhubung ke KudaIDE</h2>
        <p>Kredensial GitHub Anda berhasil disinkronkan ke aplikasi <span class="highlight">KudaIDE</span>.<br>Anda dapat menutup tab ini sekarang.</p>
        <div class="hint-box">
            Tab browser ini aman untuk ditutup. Silakan kembali ke KudaIDE.
        </div>
    </div>
    <script>
        // Coba tutup otomatis jika didukung oleh browser
        setTimeout(function() {
            try { window.close(); } catch(e) {}
        }, 2000);
    </script>
</body>
</html>"#
}

/// Render halaman HTML pesan error.
fn render_error_html(msg: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="id">
<head>
    <meta charset="utf-8">
    <title>Error Otentikasi — KudaIDE</title>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #0f172a; color: #f8fafc; display: flex; align-items: center; justify-content: center; height: 100vh; margin: 0; }}
        .card {{ background: #1e293b; border: 1px solid #ef4444; border-radius: 14px; padding: 32px; max-width: 440px; text-align: center; box-shadow: 0 10px 25px rgba(0,0,0,0.5); }}
        .icon {{ width: 56px; height: 56px; border-radius: 50%; background: rgba(239,68,68,0.15); border: 2px solid #ef4444; color: #f87171; font-size: 28px; line-height: 52px; margin: 0 auto 16px; }}
        h2 {{ color: #f87171; margin: 0 0 10px; font-size: 20px; }}
        p {{ color: #94a3b8; font-size: 13px; line-height: 1.6; margin: 0; }}
    </style>
</head>
<body>
    <div class="card">
        <div class="icon">✕</div>
        <h2>Gagal Memproses Otentikasi</h2>
        <p>{}</p>
    </div>
</body>
</html>"#,
        msg
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pickup_query_pk() {
        assert_eq!(
            parse_pickup_from_query("pk=pk_abc1234567890123456"),
            Some("pk_abc1234567890123456".to_string())
        );
        assert_eq!(
            parse_pickup_from_query("pickup=pk_xyz789def0123456"),
            Some("pk_xyz789def0123456".to_string())
        );
        assert_eq!(
            parse_pickup_from_query("code=return123&pk=pk_test123456789012&foo=bar"),
            Some("pk_test123456789012".to_string())
        );
        assert_eq!(
            parse_pickup_from_query("pk=pk_percent%30encoded_123456"),
            Some("pk_percent0encoded_123456".to_string())
        );
        assert_eq!(parse_pickup_from_query("foo=bar"), None);
        assert_eq!(parse_pickup_from_query(""), None);
    }

    #[test]
    fn parses_pickup_json_body() {
        assert_eq!(
            parse_pickup_from_body(r#"{"pickup":"pk_abc1234567890123456"}"#),
            Some("pk_abc1234567890123456".to_string())
        );
        assert_eq!(
            parse_pickup_from_body(r#"{"pk":"pk_xyz789def0123456"}"#),
            Some("pk_xyz789def0123456".to_string())
        );
        assert_eq!(
            parse_pickup_from_body(r#"  { "pickup": "pk_spaced_1234567890" }  "#),
            Some("pk_spaced_1234567890".to_string())
        );
        assert_eq!(parse_pickup_from_body(r#"{"foo":"bar"}"#), None);
        assert_eq!(parse_pickup_from_body(""), None);
    }

    #[test]
    fn parses_pickup_form_body() {
        assert_eq!(
            parse_pickup_from_body("pk=pk_form123456789012345"),
            Some("pk_form123456789012345".to_string())
        );
        assert_eq!(
            parse_pickup_from_body("pickup=pk_form123456789012345&other=1"),
            Some("pk_form123456789012345".to_string())
        );
    }

    #[test]
    fn extracts_valid_pk_filters_invalid() {
        assert_eq!(extract_valid_pk("pk_short"), None);
        assert_eq!(extract_valid_pk("not_pk_123456789012345"), None);
        assert_eq!(
            extract_valid_pk("pk_valid_long_secret_12345"),
            Some("pk_valid_long_secret_12345".to_string())
        );
    }
}

