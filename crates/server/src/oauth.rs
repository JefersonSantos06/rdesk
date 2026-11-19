//! Google and GitHub sign-in (OAuth 2 authorization code with PKCE). All we keep from the
//! provider is the account's id and its *verified* e-mail addresses: an account logs in only
//! if one of them was allowed by an admin.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::OAuthApp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Google,
    GitHub,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "google" => Some(Self::Google),
            "github" => Some(Self::GitHub),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::GitHub => "github",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::GitHub => "GitHub",
        }
    }

    fn auth_url(self) -> &'static str {
        match self {
            Self::Google => "https://accounts.google.com/o/oauth2/v2/auth",
            Self::GitHub => "https://github.com/login/oauth/authorize",
        }
    }

    fn token_url(self) -> &'static str {
        match self {
            Self::Google => "https://oauth2.googleapis.com/token",
            Self::GitHub => "https://github.com/login/oauth/access_token",
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Self::Google => "openid email profile",
            Self::GitHub => "read:user user:email",
        }
    }
}

/// PKCE: a random verifier and its S256 challenge.
pub fn pkce_pair() -> (String, String) {
    let verifier = crate::db::random_token();
    (verifier.clone(), pkce_challenge(&verifier))
}

pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Where to send the browser.
pub fn authorize_url(p: Provider, app: &OAuthApp, redirect: &str, state: &str, challenge: &str) -> Result<String> {
    let mut u = url::Url::parse(p.auth_url())?;
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &app.client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", p.scope())
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    if p == Provider::Google {
        // Always show the account chooser: a family PC may have several Google accounts.
        u.query_pairs_mut().append_pair("prompt", "select_account");
    }
    Ok(u.into())
}

/// Who signed in.
#[derive(Debug, Clone)]
pub struct Account {
    pub subject: String,
    pub name: String,
    pub verified_emails: Vec<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn finish(
    http: &reqwest::Client,
    p: Provider,
    app: &OAuthApp,
    redirect: &str,
    code: &str,
    verifier: &str,
) -> Result<Account> {
    let tok: TokenAnswer = http
        .post(p.token_url())
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", &app.client_id),
            ("client_secret", &app.client_secret),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("contacting the sign-in provider")?
        .json()
        .await
        .context("reading the sign-in provider's answer")?;
    let token = match (tok.access_token, tok.error) {
        (Some(t), _) => t,
        (None, e) => bail!(
            "{} refused the sign-in: {}",
            p.label(),
            tok.error_description.or(e).unwrap_or_else(|| "no token".into())
        ),
    };
    match p {
        Provider::Google => google_account(http, &token).await,
        Provider::GitHub => github_account(http, &token).await,
    }
}

async fn google_account(http: &reqwest::Client, token: &str) -> Result<Account> {
    #[derive(Deserialize)]
    struct Info {
        sub: String,
        email: Option<String>,
        email_verified: Option<bool>,
        name: Option<String>,
    }
    let i: Info = http
        .get("https://openidconnect.googleapis.com/v1/userinfo")
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let verified = match (i.email, i.email_verified) {
        (Some(e), Some(true)) => vec![e.to_lowercase()],
        _ => Vec::new(),
    };
    Ok(Account { subject: i.sub, name: i.name.unwrap_or_default(), verified_emails: verified })
}

async fn github_account(http: &reqwest::Client, token: &str) -> Result<Account> {
    #[derive(Deserialize)]
    struct UserInfo {
        id: u64,
        login: String,
        name: Option<String>,
    }
    #[derive(Deserialize)]
    struct Email {
        email: String,
        verified: bool,
    }
    let get = |path: &str| {
        http.get(format!("https://api.github.com{path}"))
            .bearer_auth(token)
            .header("User-Agent", "rdesk-server")
            .header("Accept", "application/vnd.github+json")
            .send()
    };
    let u: UserInfo = get("/user").await?.error_for_status()?.json().await?;
    let emails: Vec<Email> = get("/user/emails").await?.error_for_status()?.json().await?;
    Ok(Account {
        subject: u.id.to_string(),
        name: u.name.filter(|n| !n.is_empty()).unwrap_or(u.login),
        verified_emails: emails.into_iter().filter(|e| e.verified).map(|e| e.email.to_lowercase()).collect(),
    })
}

/// Decide which allowed user this account is. An account already linked to a user stays
/// with it; otherwise one of its verified e-mails must be on the allow list.
pub fn match_user(db: &crate::db::Db, p: Provider, acc: &Account) -> Result<crate::db::User> {
    if let Some(id) = db.identity_user(p.id(), &acc.subject)? {
        let u = db.user(id)?.ok_or_else(|| anyhow!("this account is no longer allowed"))?;
        return Ok(u);
    }
    for e in &acc.verified_emails {
        if let Some(u) = db.user_by_email(e)? {
            db.link_identity(p.id(), &acc.subject, u.id)?;
            return Ok(u);
        }
    }
    if acc.verified_emails.is_empty() {
        bail!("your {} account has no verified e-mail", p.label());
    }
    bail!("{} is not allowed on this server; ask an admin to add it", acc.verified_emails.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        // RFC 7636, appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn linked_accounts_and_allowed_emails() {
        let db = crate::db::Db::memory().unwrap();
        let ana = db.add_user("ana@example.com", false).unwrap();
        let acc = |sub: &str, emails: &[&str]| Account {
            subject: sub.into(),
            name: String::new(),
            verified_emails: emails.iter().map(|s| s.to_string()).collect(),
        };
        assert!(match_user(&db, Provider::GitHub, &acc("1", &["x@example.com"])).is_err());
        assert_eq!(match_user(&db, Provider::GitHub, &acc("1", &["x@y.z", "ana@example.com"])).unwrap().id, ana.id);
        // Linked now: found by account id even if the e-mail changed.
        assert_eq!(match_user(&db, Provider::GitHub, &acc("1", &[])).unwrap().id, ana.id);
        // Removing the user ends it.
        db.remove_user(ana.id).unwrap();
        assert!(match_user(&db, Provider::GitHub, &acc("1", &["ana@example.com"])).is_err());
    }
}
