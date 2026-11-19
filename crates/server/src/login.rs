//! Browser sign-in: Google/GitHub (or dev login on a local test server), then the second
//! factor (TOTP or passkey; set up on the first sign-in), then back to the app with a
//! one-time code the app trades for a session (`POST /api/token`, PKCE-checked).
//!
//! The sign-in is tied to the browser by the `rdesk_flow` cookie (SameSite=Lax, so other
//! sites cannot post into it).

use std::time::Instant;

use axum::Json;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use webauthn_rs::prelude::{Passkey, PublicKeyCredential, RegisterPublicKeyCredential, Uuid};

use crate::db::{self, User};
use crate::oauth::{self, Provider};
use crate::pages::{self, PASSKEY_JS, esc};
use crate::state::{AppRequest, Flow, MAX_TOTP_FAILURES, PendingCode, Shared};

const COOKIE: &str = "rdesk_flow";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn flow_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

fn fail(st: &Shared, msg: impl AsRef<str>) -> Response {
    (StatusCode::BAD_REQUEST, pages::error(&st.cfg.name, msg.as_ref())).into_response()
}

fn json_fail(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(rdesk_proto::api::ApiError { error: msg.into() })).into_response()
}

/// Run `f` on this browser's sign-in, if it has one that is still alive.
fn with_flow<T>(st: &Shared, headers: &HeaderMap, f: impl FnOnce(&mut Flow) -> T) -> Option<T> {
    let id = flow_id(headers)?;
    let mut flows = st.flows.lock().unwrap();
    let flow = flows.get_mut(&id)?;
    if flow.created.elapsed() > crate::state::FLOW_TTL {
        flows.remove(&id);
        return None;
    }
    Some(f(flow))
}

const EXPIRED: &str = "This sign-in expired or was opened in another browser.";
const NO_PASSKEYS: &str = "Passkeys are off on this server (it needs a domain name).";

/// The user this sign-in has identified (after Google/GitHub), if allowed.
fn flow_user(st: &Shared, headers: &HeaderMap) -> Result<User, String> {
    let uid = with_flow(st, headers, |f| f.user_id).ok_or(EXPIRED)?;
    let uid = uid.ok_or("Choose how to sign in first.")?;
    let user = st.db.user(uid).map_err(|e| e.to_string())?.ok_or("This account was removed.")?;
    if user.disabled {
        return Err("This account is disabled.".into());
    }
    Ok(user)
}

/// Only plain-HTTP loopback redirects: the code goes to the app on this computer.
fn valid_app_redirect(uri: &str) -> bool {
    let Ok(u) = url::Url::parse(uri) else { return false };
    u.scheme() == "http"
        && u.port().is_some()
        && u.fragment().is_none()
        && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

/// Second factor done: hand a one-time code to the app.
fn finish(st: &Shared, headers: &HeaderMap, user: &User, how: &str) -> String {
    let app = match with_flow(st, headers, |f| f.app.clone()) {
        Some(a) => a,
        None => return st.url("/login"),
    };
    if let Some(id) = flow_id(headers) {
        st.flows.lock().unwrap().remove(&id);
    }
    let code = db::random_token();
    st.codes.lock().unwrap().insert(
        code.clone(),
        PendingCode {
            created: Instant::now(),
            user_id: user.id,
            code_challenge: app.code_challenge.clone(),
            device: app.device.clone(),
        },
    );
    st.db.audit(Some(user.id), "login", &format!("{how} · {}", app.device));
    let _ = st.db.touch_login(user.id);
    let mut u = url::Url::parse(&app.redirect_uri).expect("checked when the sign-in started");
    u.query_pairs_mut().append_pair("code", &code).append_pair("state", &app.state);
    u.into()
}

// ---------------------------------------------------------------------------
// start + identify
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct Start {
    redirect_uri: String,
    state: String,
    code_challenge: String,
    #[serde(default)]
    device: String,
}

/// `GET /login`: opened by the app in the browser.
pub async fn start(State(st): State<Shared>, Query(q): Query<Start>) -> Response {
    st.sweep();
    if !valid_app_redirect(&q.redirect_uri) {
        return fail(&st, "Invalid redirect: sign in from the rdesk app.");
    }
    if q.state.is_empty() || q.state.len() > 200 || q.code_challenge.len() != 43 {
        return fail(&st, "Invalid sign-in request: start again from the rdesk app.");
    }
    let id = db::random_token();
    let device: String = q.device.chars().take(80).collect();
    st.flows.lock().unwrap().insert(
        id.clone(),
        Flow {
            created: Instant::now(),
            app: AppRequest {
                redirect_uri: q.redirect_uri,
                state: q.state,
                code_challenge: q.code_challenge,
                device: if device.trim().is_empty() { "unknown device".into() } else { device },
            },
            oauth: None,
            user_id: None,
            totp_setup: None,
            totp_failures: 0,
            passkey_reg: None,
            passkey_auth: None,
        },
    );
    let mut body =
        String::from("<h1>Sign in</h1><p class=\"muted\">Only e-mails an admin has allowed can sign in.</p>");
    for p in st.cfg.providers() {
        match p.as_str() {
            "dev" => body.push_str(
                r#"<hr><form method="post" action="/login/dev"><p class="muted">Test login (local server only)</p>
                <input type="email" name="email" placeholder="allowed e-mail" required autofocus>
                <button class="btn" type="submit">Continue</button></form>"#,
            ),
            other => {
                let label = Provider::parse(other).map(Provider::label).unwrap_or(other);
                body.push_str(&format!(
                    r#"<a class="btn primary" href="/login/oauth/{other}">Continue with {}</a>"#,
                    esc(label)
                ));
            }
        }
    }
    if st.cfg.providers().is_empty() {
        body.push_str("<p class=\"err\">No sign-in method is configured on this server.</p>");
    }
    let secure = if st.cfg.is_https() { "; Secure" } else { "" };
    let cookie = format!("{COOKIE}={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=900{secure}");
    ([(header::SET_COOKIE, cookie)], pages::page(&st.cfg.name, "Sign in", &body)).into_response()
}

/// `GET /login/oauth/{provider}`: off to Google/GitHub.
pub async fn oauth_go(State(st): State<Shared>, headers: HeaderMap, Path(p): Path<String>) -> Response {
    let Some(provider) = Provider::parse(&p) else { return fail(&st, "Unknown sign-in method.") };
    let Some(app) = oauth_app(&st, provider) else { return fail(&st, "This sign-in method is not configured.") };
    let (verifier, challenge) = oauth::pkce_pair();
    let state = db::random_token();
    let ok = with_flow(&st, &headers, |f| f.oauth = Some((provider, state.clone(), verifier))).is_some();
    if !ok {
        return fail(&st, EXPIRED);
    }
    let redirect = st.url(&format!("/login/oauth/{}/callback", provider.id()));
    match oauth::authorize_url(provider, &app, &redirect, &state, &challenge) {
        Ok(u) => Redirect::to(&u).into_response(),
        Err(e) => fail(&st, format!("{e:#}")),
    }
}

fn oauth_app(st: &Shared, p: Provider) -> Option<crate::config::OAuthApp> {
    match p {
        Provider::Google => st.cfg.google.clone(),
        Provider::GitHub => st.cfg.github.clone(),
    }
}

#[derive(Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /login/oauth/{provider}/callback`.
pub async fn oauth_back(
    State(st): State<Shared>,
    headers: HeaderMap,
    Path(p): Path<String>,
    Query(q): Query<Callback>,
) -> Response {
    let Some(provider) = Provider::parse(&p) else { return fail(&st, "Unknown sign-in method.") };
    if let Some(e) = q.error {
        return fail(&st, format!("{} said: {e}", provider.label()));
    }
    let pending = with_flow(&st, &headers, |f| f.oauth.take());
    let Some(Some((expected, state, verifier))) = pending else { return fail(&st, EXPIRED) };
    if expected != provider || q.state.as_deref() != Some(state.as_str()) {
        return fail(&st, "The sign-in answer does not match this browser. Start again.");
    }
    let (Some(code), Some(app)) = (q.code, oauth_app(&st, provider)) else {
        return fail(&st, "No authorization code.");
    };
    let redirect = st.url(&format!("/login/oauth/{}/callback", provider.id()));
    let account = match oauth::finish(&st.http, provider, &app, &redirect, &code, &verifier).await {
        Ok(a) => a,
        Err(e) => return fail(&st, format!("{e:#}")),
    };
    let user = match oauth::match_user(&st.db, provider, &account) {
        Ok(u) if u.disabled => return fail(&st, "This account is disabled."),
        Ok(u) => u,
        Err(e) => {
            st.db.audit(None, "login-refused", &format!("{}: {e:#}", provider.id()));
            return fail(&st, format!("{e:#}"));
        }
    };
    if !account.name.is_empty() {
        let _ = st.db.set_name_if_empty(user.id, &account.name);
    }
    identified(&st, &headers, &user)
}

#[derive(Deserialize)]
pub struct DevLogin {
    email: String,
}

/// `POST /login/dev`: only on a local test server (`dev_login`).
pub async fn dev(State(st): State<Shared>, headers: HeaderMap, Form(f): Form<DevLogin>) -> Response {
    if !st.cfg.dev_login_enabled() {
        return fail(&st, "Test login is off.");
    }
    match st.db.user_by_email(&f.email) {
        Ok(Some(u)) if !u.disabled => identified(&st, &headers, &u),
        _ => fail(&st, format!("{} is not allowed on this server.", f.email.trim())),
    }
}

fn identified(st: &Shared, headers: &HeaderMap, user: &User) -> Response {
    let ok = with_flow(st, headers, |f| {
        f.user_id = Some(user.id);
        f.totp_failures = 0;
    })
    .is_some();
    if !ok {
        return fail(st, EXPIRED);
    }
    Redirect::to("/login/2fa").into_response()
}

// ---------------------------------------------------------------------------
// second factor
// ---------------------------------------------------------------------------

fn passkeys_of(st: &Shared, user: &User) -> Vec<(i64, Passkey)> {
    st.db
        .passkeys(user.id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| serde_json::from_str(&p.data).ok().map(|k| (p.id, k)))
        .collect()
}

/// `GET /login/2fa`: verify, or set up the first factor.
pub async fn second_factor(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return fail(&st, e),
    };
    let has_totp = user.totp_secret.is_some();
    let has_passkey = st.webauthn.is_some() && !passkeys_of(&st, &user).is_empty();
    let who = esc(&user.email);
    let body = if !has_totp && !has_passkey {
        let passkey = if st.webauthn.is_some() {
            format!(r#"<button class="btn primary" onclick="passkeyRegister()">Create a passkey</button>{PASSKEY_JS}"#)
        } else {
            String::new()
        };
        format!(
            r#"<h1>Protect your account</h1><p>Signed in as <b>{who}</b>. Set up a second factor; it is asked on every sign-in.</p>
            {passkey}<a class="btn" href="/login/2fa/totp-setup">Use an authenticator app (TOTP)</a>
            <p id="pkerr" class="err"></p>"#
        )
    } else {
        let mut b = format!(r#"<h1>Second factor</h1><p>Signed in as <b>{who}</b>.</p>"#);
        if has_passkey {
            b.push_str(r#"<button class="btn primary" onclick="passkeyLogin()">Use my passkey</button><p id="pkerr" class="err"></p>"#);
            b.push_str(PASSKEY_JS);
        }
        if has_totp {
            if has_passkey {
                b.push_str("<hr>");
            }
            b.push_str(
                r#"<form method="post" action="/login/2fa/totp"><p>Code from your authenticator app:</p>
                <input class="code" type="text" name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="6" required autofocus>
                <button class="btn primary" type="submit">Verify</button></form>"#,
            );
        }
        b
    };
    pages::page(&st.cfg.name, "Second factor", &body).into_response()
}

fn totp_for(secret_b32: &str, account: &str, issuer: &str) -> Option<totp_rs::Totp> {
    let secret = totp_rs::Secret::try_from_base32(secret_b32).ok()?;
    totp_rs::Builder::new().with_secret(secret).with_account_name(account).with_issuer(Some(issuer)).build().ok()
}

/// `GET /login/2fa/totp-setup`: show a new secret as a QR code.
pub async fn totp_setup_page(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return fail(&st, e),
    };
    if user.totp_secret.is_some() {
        return Redirect::to("/login/2fa").into_response();
    }
    let secret = with_flow(&st, &headers, |f| {
        f.totp_setup.get_or_insert_with(|| totp_rs::Secret::generate().to_base32()).clone()
    });
    let Some(secret) = secret else { return fail(&st, EXPIRED) };
    totp_setup_render(&st, &user, &secret, None)
}

fn totp_setup_render(st: &Shared, user: &User, secret: &str, error: Option<&str>) -> Response {
    let Some(totp) = totp_for(secret, &user.email, &st.cfg.name) else { return fail(st, "Could not create the code.") };
    let qr = totp.to_qr_base64().unwrap_or_default();
    let err = error.map(|e| format!(r#"<p class="err">{}</p>"#, esc(e))).unwrap_or_default();
    let body = format!(
        r#"<h1>Authenticator app</h1><p>Scan with Google Authenticator, 1Password, Authy…</p>
        <img class="qr" alt="QR code" src="data:image/png;base64,{qr}">
        <p class="muted">Or type this key: <code data-totp-secret="{s}">{s}</code></p>
        <form method="post" action="/login/2fa/totp-setup"><p>Then enter the 6-digit code it shows:</p>
        <input class="code" type="text" name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="6" required autofocus>
        {err}<button class="btn primary" type="submit">Confirm</button></form>
        <a class="btn" href="/login/2fa">Back</a>"#,
        s = esc(secret),
    );
    pages::page(&st.cfg.name, "Authenticator app", &body).into_response()
}

#[derive(Deserialize)]
pub struct CodeForm {
    code: String,
}

/// `POST /login/2fa/totp-setup`: confirm the new authenticator and finish.
pub async fn totp_setup_confirm(State(st): State<Shared>, headers: HeaderMap, Form(f): Form<CodeForm>) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return fail(&st, e),
    };
    if user.totp_secret.is_some() {
        return Redirect::to("/login/2fa").into_response();
    }
    let Some(Some(secret)) = with_flow(&st, &headers, |f| f.totp_setup.clone()) else {
        return Redirect::to("/login/2fa/totp-setup").into_response();
    };
    let Some(totp) = totp_for(&secret, &user.email, &st.cfg.name) else { return fail(&st, "Bad secret.") };
    match totp.check_current(f.code.trim()) {
        Some(step) => {
            if let Err(e) = st.db.set_totp(user.id, &secret, step as i64) {
                return fail(&st, format!("{e:#}"));
            }
            st.db.audit(Some(user.id), "2fa-setup", "totp");
            Redirect::to(&finish(&st, &headers, &user, "totp (new)")).into_response()
        }
        None => {
            if too_many_failures(&st, &headers) {
                return fail(&st, "Too many wrong codes. Start again.");
            }
            totp_setup_render(
                &st,
                &user,
                &secret,
                Some("That code is not right; check the phone's clock and try again."),
            )
        }
    }
}

fn too_many_failures(st: &Shared, headers: &HeaderMap) -> bool {
    let over = with_flow(st, headers, |f| {
        f.totp_failures += 1;
        f.totp_failures >= MAX_TOTP_FAILURES
    })
    .unwrap_or(true);
    if over && let Some(id) = flow_id(headers) {
        st.flows.lock().unwrap().remove(&id);
    }
    over
}

/// `POST /login/2fa/totp`: check the code and finish.
pub async fn totp_verify(State(st): State<Shared>, headers: HeaderMap, Form(f): Form<CodeForm>) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return fail(&st, e),
    };
    let Some(secret) = &user.totp_secret else { return Redirect::to("/login/2fa").into_response() };
    let Some(totp) = totp_for(secret, &user.email, &st.cfg.name) else { return fail(&st, "Bad secret.") };
    let ok = match totp.check_current(f.code.trim()) {
        // Each code works once (RFC 6238 §5.2).
        Some(step) => st.db.use_totp_step(user.id, step as i64).unwrap_or(false),
        None => false,
    };
    if ok {
        return Redirect::to(&finish(&st, &headers, &user, "totp")).into_response();
    }
    st.db.audit(Some(user.id), "2fa-failed", "totp");
    if too_many_failures(&st, &headers) {
        return fail(&st, "Too many wrong codes. Start again.");
    }
    let body = r#"<h1>Second factor</h1><p class="err">That code is not right (or was already used). Wait for the next one.</p>
        <form method="post" action="/login/2fa/totp">
        <input class="code" type="text" name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="6" required autofocus>
        <button class="btn primary" type="submit">Verify</button></form>"#;
    pages::page(&st.cfg.name, "Second factor", body).into_response()
}

/// `POST /login/2fa/passkey/register/start`.
pub async fn passkey_register_start(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return json_fail(e),
    };
    // Adding a passkey is part of the first setup only (later: from the account page).
    if user.totp_secret.is_some() || !passkeys_of(&st, &user).is_empty() {
        return json_fail("A second factor is already set up.");
    }
    let uuid = match st.db.webauthn_id(user.id).ok().and_then(|w| Uuid::parse_str(&w).ok()) {
        Some(u) => u,
        None => return json_fail("database error"),
    };
    let display = if user.name.is_empty() { user.email.clone() } else { user.name.clone() };
    let Some(webauthn) = &st.webauthn else { return json_fail(NO_PASSKEYS) };
    match webauthn.start_passkey_registration(uuid, &user.email, &display, None) {
        Ok((ccr, reg)) => {
            if with_flow(&st, &headers, |f| f.passkey_reg = Some(reg)).is_none() {
                return json_fail(EXPIRED);
            }
            Json(ccr).into_response()
        }
        Err(e) => json_fail(format!("passkey: {e}")),
    }
}

/// `POST /login/2fa/passkey/register/finish`.
pub async fn passkey_register_finish(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(reg): Json<RegisterPublicKeyCredential>,
) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return json_fail(e),
    };
    let Some(Some(state)) = with_flow(&st, &headers, |f| f.passkey_reg.take()) else {
        return json_fail("Start the passkey setup again.");
    };
    let Some(webauthn) = &st.webauthn else { return json_fail(NO_PASSKEYS) };
    let key = match webauthn.finish_passkey_registration(&reg, &state) {
        Ok(k) => k,
        Err(e) => return json_fail(format!("The passkey was not accepted: {e}")),
    };
    let data = match serde_json::to_string(&key) {
        Ok(d) => d,
        Err(e) => return json_fail(e.to_string()),
    };
    if let Err(e) = st.db.add_passkey(user.id, "passkey", &data) {
        return json_fail(format!("{e:#}"));
    }
    st.db.audit(Some(user.id), "2fa-setup", "passkey");
    Json(serde_json::json!({ "redirect": finish(&st, &headers, &user, "passkey (new)") })).into_response()
}

/// `POST /login/2fa/passkey/auth/start`.
pub async fn passkey_auth_start(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return json_fail(e),
    };
    let keys: Vec<Passkey> = passkeys_of(&st, &user).into_iter().map(|(_, k)| k).collect();
    if keys.is_empty() {
        return json_fail("This account has no passkey.");
    }
    let Some(webauthn) = &st.webauthn else { return json_fail(NO_PASSKEYS) };
    match webauthn.start_passkey_authentication(&keys) {
        Ok((rcr, auth)) => {
            if with_flow(&st, &headers, |f| f.passkey_auth = Some(auth)).is_none() {
                return json_fail(EXPIRED);
            }
            Json(rcr).into_response()
        }
        Err(e) => json_fail(format!("passkey: {e}")),
    }
}

/// `POST /login/2fa/passkey/auth/finish`.
pub async fn passkey_auth_finish(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(cred): Json<PublicKeyCredential>,
) -> Response {
    let user = match flow_user(&st, &headers) {
        Ok(x) => x,
        Err(e) => return json_fail(e),
    };
    let Some(Some(state)) = with_flow(&st, &headers, |f| f.passkey_auth.take()) else {
        return json_fail("Start again.");
    };
    let Some(webauthn) = &st.webauthn else { return json_fail(NO_PASSKEYS) };
    let result = match webauthn.finish_passkey_authentication(&cred, &state) {
        Ok(r) => r,
        Err(e) => {
            st.db.audit(Some(user.id), "2fa-failed", "passkey");
            return json_fail(format!("The passkey was not accepted: {e}"));
        }
    };
    // Keep the signature counter current (detects cloned authenticators).
    for (id, mut k) in passkeys_of(&st, &user) {
        if k.update_credential(&result).is_some()
            && let Ok(d) = serde_json::to_string(&k)
        {
            let _ = st.db.update_passkey(id, &d);
        }
    }
    Json(serde_json::json!({ "redirect": finish(&st, &headers, &user, "passkey") })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_app_redirects() {
        assert!(valid_app_redirect("http://127.0.0.1:53111/callback"));
        assert!(valid_app_redirect("http://localhost:9/cb"));
        assert!(!valid_app_redirect("http://127.0.0.1/callback"), "needs a port");
        assert!(!valid_app_redirect("https://evil.example.com:443/cb"));
        assert!(!valid_app_redirect("http://127.0.0.1.evil.com:80/cb"));
        assert!(!valid_app_redirect("rdesk://callback"));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; rdesk_flow=abc; b=2".parse().unwrap());
        assert_eq!(flow_id(&h).as_deref(), Some("abc"));
    }
}
