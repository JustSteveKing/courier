//! Interface language. Strings live in `locales/*.yml` (rust-i18n format v2: every key
//! lists all languages side by side) and are looked up with `t!` with a key.
//!
//! The language follows the system locale (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`)
//! unless the user picks one in settings. Low-level error details (file system, parser and
//! network errors) stay in English.
//!
//! GPUI Kit's components carry their own strings (dialog buttons, the input context menu),
//! translated only into Chinese and Italian; `crate::ui` builds components with ours.

/// Supported languages: code and native name, in the order shown to users.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("es", "Español"),
    ("de", "Deutsch"),
    ("fr", "Français"),
];

/// The language to use for a settings value (None = follow the system).
pub fn resolve(setting: Option<&str>) -> &'static str {
    setting.and_then(supported).unwrap_or_else(system_language)
}

/// The first supported language from the POSIX locale environment, else English.
pub fn system_language() -> &'static str {
    let vars = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"].map(|name| std::env::var(name).ok());
    language_from_env(&vars)
}

fn language_from_env(values: &[Option<String>]) -> &'static str {
    values
        .iter()
        .flatten()
        // LANGUAGE may hold a priority list like "de:fr:en".
        .flat_map(|value| value.split(':'))
        .find_map(supported)
        .unwrap_or("en")
}

/// Maps `de`, `de_DE.UTF-8`, `fr-CA` and similar to a supported code.
fn supported(locale: &str) -> Option<&'static str> {
    let code = locale.split(['_', '-', '.', '@']).next()?.to_ascii_lowercase();
    LANGUAGES.iter().map(|(c, _)| *c).find(|c| *c == code)
}

pub fn apply(code: &str) {
    // One global shared with GPUI Kit's own component strings.
    rust_i18n::set_locale(code);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use super::*;

    #[test]
    fn resolves_locales() {
        let env = |v: &[&str]| {
            v.iter()
                .map(|s| (!s.is_empty()).then(|| s.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(language_from_env(&env(&["", "", "", "de_DE.UTF-8"])), "de");
        assert_eq!(language_from_env(&env(&["pt:fr:en", "", "", "en_US.UTF-8"])), "fr");
        assert_eq!(language_from_env(&env(&["", "C", "", "ja_JP.UTF-8"])), "en");
        assert_eq!(resolve(Some("es")), "es");
        assert_eq!(supported("fr-CA"), Some("fr"));
        assert_eq!(supported("sr@latin"), None);
    }

    fn placeholders(text: &str) -> BTreeSet<String> {
        text.match_indices("%{")
            .filter_map(|(i, _)| text[i + 2..].split_once('}').map(|(name, _)| name.to_string()))
            .collect()
    }

    fn locale_entries() -> Vec<(String, serde_norway::Mapping)> {
        let mut entries = Vec::new();
        for file in fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("locales"))
            .unwrap()
            .flatten()
        {
            let text = fs::read_to_string(file.path()).unwrap();
            let map: serde_norway::Mapping = serde_norway::from_str(&text).unwrap();
            for (key, value) in map {
                let key = key.as_str().unwrap().to_string();
                if key == "_version" {
                    continue;
                }
                let translations = value
                    .as_mapping()
                    .cloned()
                    .unwrap_or_else(|| panic!("{key}: not a mapping"));
                entries.push((key, translations));
            }
        }
        entries
    }

    /// Every key is translated into every language, with the same `%{placeholders}`.
    #[test]
    fn every_string_is_fully_translated() {
        let entries = locale_entries();
        assert!(!entries.is_empty());
        let mut seen = BTreeSet::new();
        for (key, translations) in &entries {
            assert!(seen.insert(key.clone()), "{key} is defined twice");
            let english = translations
                .get("en")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("{key}: no en"));
            for (code, _) in LANGUAGES {
                let text = translations
                    .get(*code)
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("{key}: missing {code}"));
                assert!(!text.trim().is_empty(), "{key}: empty {code}");
                assert_eq!(
                    placeholders(text),
                    placeholders(english),
                    "{key}: {code} placeholders differ from en"
                );
            }
        }
    }

    /// Every `t!` with a key in the source has a translation entry (rust-i18n would otherwise
    /// silently show the raw key).
    #[test]
    fn every_used_key_exists() {
        let known: BTreeSet<String> = locale_entries().into_iter().map(|(k, _)| k).collect();
        let mut missing = Vec::new();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut stack = vec![root.join("src"), root.join("crates/courier-core/src")];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let text = fs::read_to_string(&path).unwrap();
                for (i, _) in text.match_indices("t!(\"") {
                    // Skip `format!("`, `print!("` and the like.
                    if text[..i]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
                    {
                        continue;
                    }
                    let rest = &text[i + 4..];
                    let key = &rest[..rest.find('"').unwrap()];
                    if !known.contains(key) {
                        missing.push(format!("{}: {key}", path.display()));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "untranslated keys:\n{}", missing.join("\n"));
    }
}
