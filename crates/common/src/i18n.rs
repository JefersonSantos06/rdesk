//! The app's two languages, English and Brazilian Portuguese.
//!
//! Every text shown to people carries both versions where it is written:
//!
//! ```
//! use rdesk_common::{t, tf};
//! let name = "PC da sala";
//! let label: &str = t!("Connect", "Conectar");
//! let toast: String = tf!("Opening {name}…", "Abrindo {name}…");
//! ```
//!
//! The language is one setting for the whole process ([`set`]), chosen in Settings or taken
//! from the system ([`resolve`]). Before anything sets it, it is English: the command line
//! and the server stay in English. Only what people read is translated, never what goes to
//! the server, to logs or over the wire.

use std::sync::atomic::{AtomicBool, Ordering};

use rdesk_proto::Access;
use rdesk_proto::api::{ShareRole, Visibility};
use serde::{Deserialize, Serialize};

/// A language the app speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Pt,
}

/// The language chosen in Settings (`language` in client.toml).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LanguagePref {
    /// The system's language (Portuguese if it is Portuguese, English otherwise).
    #[default]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "pt-br")]
    Portuguese,
}

static PORTUGUESE: AtomicBool = AtomicBool::new(false);

/// Tests that read or change the language hold this (it is one setting for the process).
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Speak `lang` from now on (the whole process).
pub fn set(lang: Lang) {
    PORTUGUESE.store(lang == Lang::Pt, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if is_pt() { Lang::Pt } else { Lang::En }
}

pub fn is_pt() -> bool {
    PORTUGUESE.load(Ordering::Relaxed)
}

/// The text in the current language (what [`t!`](crate::t) expands to).
pub fn pick(en: &'static str, pt: &'static str) -> &'static str {
    if is_pt() { pt } else { en }
}

/// The language a preference means here.
pub fn resolve(pref: LanguagePref) -> Lang {
    match pref {
        LanguagePref::English => Lang::En,
        LanguagePref::Portuguese => Lang::Pt,
        LanguagePref::System => system(),
    }
}

/// Set the language from client.toml (or the system's).
pub fn init_from_config() {
    let pref = crate::config::load_toml::<crate::config::ClientConfig>(crate::config::CLIENT_CONFIG)
        .map(|c| c.app.language)
        .unwrap_or_default();
    set(resolve(pref));
}

/// Portuguese if the system's language is, English otherwise.
pub fn system() -> Lang {
    match system_locale() {
        Some(l) if is_portuguese(&l) => Lang::Pt,
        _ => Lang::En,
    }
}

fn is_portuguese(locale: &str) -> bool {
    let l = locale.trim().to_ascii_lowercase();
    l == "pt" || l.starts_with("pt-") || l.starts_with("pt_") || l.starts_with("pt.")
}

/// The user's locale ("pt-BR", "en_US.UTF-8"…), if it can be found.
fn system_locale() -> Option<String> {
    // Set explicitly (also how a terminal session says it).
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
            && v != "C"
            && v != "POSIX"
        {
            return Some(v);
        }
    }
    platform_locale()
}

#[cfg(windows)]
fn platform_locale() -> Option<String> {
    // The display language the user chose (not the regional format).
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // Primary language id: the low 10 bits; LANG_PORTUGUESE = 0x16.
    let id = unsafe { GetUserDefaultUILanguage() } & 0x3ff;
    Some(if id == 0x16 { "pt".into() } else { "en".into() })
}

#[cfg(target_os = "macos")]
fn platform_locale() -> Option<String> {
    // Apps opened from the Finder have no LANG: ask for the first preferred language.
    let out = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split('"').nth(1).map(str::to_string)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_locale() -> Option<String> {
    None
}

/// The text for one of `t!`'s two versions.
#[macro_export]
macro_rules! t {
    ($en:literal, $pt:literal $(,)?) => {
        $crate::i18n::pick($en, $pt)
    };
}

/// Like `format!`, with the English and the Portuguese format strings (arguments in both
/// may be captured names, `{name}`, or positional, given after them).
#[macro_export]
macro_rules! tf {
    ($en:literal, $pt:literal $(, $($arg:tt)*)?) => {
        if $crate::i18n::is_pt() {
            format!($pt $(, $($arg)*)?)
        } else {
            format!($en $(, $($arg)*)?)
        }
    };
}

/// A share's role, as people read it.
pub fn role_label(r: ShareRole) -> &'static str {
    match r {
        ShareRole::Control => t!("full control", "controle total"),
        ShareRole::View => t!("view only", "só visualizar"),
        ShareRole::Files => t!("files only", "só arquivos"),
        ShareRole::Terminal => t!("terminal only", "só terminal"),
    }
}

/// What a session may do, as people read it.
pub fn access_label(a: Access) -> String {
    if !is_pt() {
        return a.label();
    }
    match a {
        Access::FULL => "controle total".into(),
        Access::VIEW => "só visualizar".into(),
        Access::FILES => "só arquivos".into(),
        Access::TERMINAL => "só terminal".into(),
        Access::NONE => "sem acesso".into(),
        a => {
            let mut parts = Vec::new();
            if a.screen {
                parts.push("tela");
            }
            if a.input {
                parts.push("controle");
            }
            if a.files {
                parts.push("arquivos");
            }
            if a.terminal {
                parts.push("terminal");
            }
            parts.join(" + ")
        }
    }
}

/// Who may see a computer, as people read it.
pub fn visibility_label(v: Visibility) -> &'static str {
    match v {
        Visibility::Private => t!("only me", "só eu"),
        Visibility::Shared => t!("shared", "compartilhado"),
        Visibility::Everyone => t!("everyone", "todos"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales() {
        for l in ["pt", "pt-BR", "pt_BR.UTF-8", "PT-br", "pt-PT"] {
            assert!(is_portuguese(l), "{l}");
        }
        for l in ["en", "en-US", "es_ES", "C", "ptx", ""] {
            assert!(!is_portuguese(l), "{l}");
        }
    }

    #[test]
    fn macros_and_language() {
        let _lang = TEST_LOCK.lock().unwrap();
        let name = "PC";
        set(Lang::En);
        assert_eq!(t!("Connect", "Conectar"), "Connect");
        assert_eq!(tf!("Opening {name}…", "Abrindo {name}…"), "Opening PC…");
        assert_eq!(tf!("{} of {}", "{} de {}", 1, 2), "1 of 2");
        assert_eq!(role_label(ShareRole::View), "view only");
        set(Lang::Pt);
        assert_eq!(t!("Connect", "Conectar"), "Conectar");
        assert_eq!(tf!("Opening {name}…", "Abrindo {name}…"), "Abrindo PC…");
        assert_eq!(tf!("{} of {}", "{} de {}", 1, 2), "1 de 2");
        assert_eq!(access_label(Access::FULL), "controle total");
        set(Lang::En);
    }

    #[test]
    fn preference_in_toml() {
        #[derive(Serialize, Deserialize)]
        struct S {
            language: LanguagePref,
        }
        let s: S = toml::from_str("language = \"pt-br\"").unwrap();
        assert_eq!(s.language, LanguagePref::Portuguese);
        let s: S = toml::from_str("language = \"system\"").unwrap();
        assert_eq!(s.language, LanguagePref::System);
        assert_eq!(toml::to_string(&S { language: LanguagePref::English }).unwrap().trim(), "language = \"en\"");
    }
}
