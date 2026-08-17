//! Loopback HTTP server untuk OAuth handoff anti-clipboard.
//!
//! Pattern standar desktop OAuth (dipakai VS Code, GitHub Desktop): saat login
//! dimulai, IDE spin-up server sementara di `127.0.0.1:PORT`. Halaman callback
//! hub (di browser) POST `pickup` code langsung ke server lokal ini. IDE terima
//! POST → pakai pickup → poll hub untuk token → login selesai. Tanpa clipboard,
//! tanpa user gesture, tanpa copas.
//!
//! Server cuma menerima POST `/pickup` dengan body JSON `{"pickup":"pk_..."}`.
//! CORS header di-set supaya browser (cross-origin dari kuda-ide.my.id) tidak
//! diblok. Bind hanya ke 127.0.0.1 (loopback) — tidak pernah expose ke network.

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
    /// Slot pickup code yang dikirim browser (diisi saat POST `/pickup` diterima).
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

                // Baca request (cukup kecil: POST /pickup JSON).
                let mut buf = vec![0u8; 4096];
                let n = match stream.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();

                // POST /pickup → simpan pickup code, emit event ke frontend.
                // OPTIONS (preflight CORS) → balas 204 + header CORS.
                let (status, body_out) = if raw.starts_with("OPTIONS") {
                    ("204 No Content", String::new())
                } else if raw.starts_with("POST /pickup") {
                    // Body ada setelah \r\n\r\n.
                    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
                    let parsed = parse_pickup(body);
                    if let Some(pk) = parsed.as_ref() {
                        *pickup_clone.lock().await = Some(pk.clone());
                        let _ = app_clone.emit("auth:pickup", pk.clone());
                        ("200 OK", "{\"ok\":true}".to_string())
                    } else {
                        ("400 Bad Request", "{\"error\":\"no pickup\"}".to_string())
                    }
                } else {
                    ("404 Not Found", "{\"error\":\"not found\"}".to_string())
                };

                let cors = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\n";
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{cors}Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
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

/// Parse `{"pickup":"pk_..."}` dari body JSON (manual, tanpa parser — body kecil).
fn parse_pickup(body: &str) -> Option<String> {
    // Cari "pickup" lalu ambil string value berikutnya.
    let key = "\"pickup\"";
    let idx = body.find(key)?;
    let after = &body[idx + key.len()..];
    let q1 = after.find('"')?;
    let rest = &after[q1 + 1..];
    let q2 = rest.find('"')?;
    Some(rest[..q2].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pickup_json() {
        assert_eq!(
            parse_pickup(r#"{"pickup":"pk_abc123"}"#),
            Some("pk_abc123".to_string())
        );
        assert_eq!(
            parse_pickup(r#"  {"pickup":  "pk_xyz789def012"  }  "#),
            Some("pk_xyz789def012".to_string())
        );
        assert_eq!(parse_pickup(r#"{"foo":"bar"}"#), None);
        assert_eq!(parse_pickup(""), None);
    }
}
