//! The few HTML pages of the browser sign-in. Plain server-rendered HTML; the only script
//! is the passkey ceremony (WebAuthn needs JavaScript).

use axum::response::Html;

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

const STYLE: &str = r#"
:root { color-scheme: light dark; --bg:#f6f7f9; --card:#fff; --fg:#1d2330; --muted:#667085; --accent:#2f6fed; --err:#c4332b; --border:#d9dde5; }
@media (prefers-color-scheme: dark) { :root { --bg:#14171c; --card:#1d2128; --fg:#e7e9ee; --muted:#9aa3b2; --accent:#5b8dff; --err:#ff6b61; --border:#2e3440; } }
* { box-sizing: border-box; }
body { margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center; background:var(--bg); color:var(--fg); font:16px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; padding:16px; }
main { width:100%; max-width:420px; background:var(--card); border:1px solid var(--border); border-radius:14px; padding:28px; }
h1 { font-size:22px; margin:0 0 4px; } p { margin:8px 0; } .muted { color:var(--muted); font-size:14px; }
.btn { display:block; width:100%; text-align:center; padding:11px 14px; margin:10px 0; border-radius:10px; border:1px solid var(--border); background:transparent; color:var(--fg); font:inherit; cursor:pointer; text-decoration:none; }
.btn.primary { background:var(--accent); border-color:var(--accent); color:#fff; }
input[type=text], input[type=email] { width:100%; padding:10px 12px; border-radius:10px; border:1px solid var(--border); background:transparent; color:var(--fg); font:inherit; }
input.code { letter-spacing:6px; font-size:22px; text-align:center; font-variant-numeric:tabular-nums; }
.err { color:var(--err); } .qr { display:block; margin:12px auto; width:200px; height:200px; image-rendering:pixelated; background:#fff; border-radius:8px; }
code { word-break:break-all; font-size:13px; } hr { border:none; border-top:1px solid var(--border); margin:18px 0; }
"#;

pub fn page(server: &str, title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{t} · {s}</title><style>{STYLE}</style></head><body><main><p class="muted">{s}</p>{body}</main></body></html>"#,
        t = esc(title),
        s = esc(server),
    ))
}

pub fn error(server: &str, msg: &str) -> Html<String> {
    page(
        server,
        "Sign-in failed",
        &format!(
            r#"<h1>Sign-in failed</h1><p class="err">{}</p><p class="muted">Close this tab and start again from the rdesk app.</p>"#,
            esc(msg)
        ),
    )
}

/// Base64url ⇄ ArrayBuffer helpers and the two WebAuthn ceremonies.
pub const PASSKEY_JS: &str = r#"
<script>
const b64u = s => Uint8Array.from(atob(s.replace(/-/g,'+').replace(/_/g,'/') + '==='.slice((s.length + 3) % 4)), c => c.charCodeAt(0));
const tob64u = b => btoa(String.fromCharCode(...new Uint8Array(b))).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'');
async function post(url, body) {
  const r = await fetch(url, {method:'POST', headers:{'Content-Type':'application/json'}, body: body ? JSON.stringify(body) : '{}'});
  const j = await r.json().catch(() => ({error: 'bad answer from the server'}));
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}
function fail(e) { const el = document.getElementById('pkerr'); el.textContent = (e && e.message) || String(e); }
async function passkeyRegister() {
  try {
    const o = await post('/login/2fa/passkey/register/start');
    o.publicKey.challenge = b64u(o.publicKey.challenge);
    o.publicKey.user.id = b64u(o.publicKey.user.id);
    (o.publicKey.excludeCredentials || []).forEach(c => c.id = b64u(c.id));
    const c = await navigator.credentials.create(o);
    const r = await post('/login/2fa/passkey/register/finish', {
      id: c.id, rawId: tob64u(c.rawId), type: c.type, extensions: c.getClientExtensionResults(),
      response: { attestationObject: tob64u(c.response.attestationObject), clientDataJSON: tob64u(c.response.clientDataJSON) } });
    location.href = r.redirect;
  } catch (e) { fail(e); }
}
async function passkeyLogin() {
  try {
    const o = await post('/login/2fa/passkey/auth/start');
    o.publicKey.challenge = b64u(o.publicKey.challenge);
    (o.publicKey.allowCredentials || []).forEach(c => c.id = b64u(c.id));
    const c = await navigator.credentials.get(o);
    const r = await post('/login/2fa/passkey/auth/finish', {
      id: c.id, rawId: tob64u(c.rawId), type: c.type, extensions: c.getClientExtensionResults(),
      response: { authenticatorData: tob64u(c.response.authenticatorData), clientDataJSON: tob64u(c.response.clientDataJSON),
                  signature: tob64u(c.response.signature), userHandle: c.response.userHandle ? tob64u(c.response.userHandle) : null } });
    location.href = r.redirect;
  } catch (e) { fail(e); }
}
</script>"#;
