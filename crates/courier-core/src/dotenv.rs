//! Reading a project's `.env` files so they can be used as environments.
//!
//! These files belong to the project, not to Courier: they're read, never written, and their
//! values stay where they are. What they hold is often secret, so anything that looks like a
//! credential is reported as one and handled like a secret everywhere else.

use std::path::{Path, PathBuf};

use crate::model::Variables;

/// A `.env` file that can be picked as an environment.
#[derive(Clone, Debug, PartialEq)]
pub struct DotEnv {
    pub path: PathBuf,
    /// What to call it in the picker: the file's own name, like `.env.local`.
    pub name: String,
}

/// The `.env` files in `project_dir`, in a sensible order: `.env` first, then the rest by
/// name. Example files are left out — they hold placeholders, not values.
pub fn files_in(project_dir: &Path) -> Vec<DotEnv> {
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return Vec::new();
    };
    let mut found: Vec<DotEnv> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let usable = name == ".env" || name.starts_with(".env.");
            let example = name.ends_with(".example") || name.ends_with(".sample") || name.ends_with(".template");
            (usable && !example).then_some(DotEnv { path, name })
        })
        .collect();
    found.sort_by(|a, b| (a.name != ".env", &a.name).cmp(&(b.name != ".env", &b.name)));
    found
}

/// Parses `.env` contents: `NAME=value` a line, `export` allowed, `#` comments, single or
/// double quotes, and `\n` escapes inside double quotes.
pub fn parse(text: &str) -> Variables {
    let mut variables = Variables::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            continue;
        }
        variables.insert(name.to_string(), unquote(value.trim()));
    }
    variables
}

fn unquote(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
        return inner.replace("\\n", "\n").replace("\\\"", "\"").replace("\\\\", "\\");
    }
    if let Some(inner) = value.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')) {
        return inner.to_string();
    }
    // An unquoted value ends at a comment, as long as there's a space before the `#`.
    match value.split_once(" #") {
        Some((before, _)) => before.trim_end().to_string(),
        None => value.to_string(),
    }
}

/// Reads a `.env` file, or nothing if it has gone away.
pub fn read(path: &Path) -> Variables {
    std::fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// The names in a `.env` file that look like credentials, so the rest of Courier can treat
/// them the way it treats secrets.
pub fn secret_names(variables: &Variables) -> Vec<String> {
    variables
        .iter()
        .filter(|(name, _)| looks_secret(name))
        .map(|(name, _)| name.clone())
        .collect()
}

fn looks_secret(name: &str) -> bool {
    const WORDS: [&str; 8] = [
        "secret",
        "token",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "private",
        "credential",
    ];
    let name = name.to_ascii_lowercase();
    WORDS.iter().any(|word| name.contains(word)) || name.ends_with("_key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_env_files_come_in() {
        let variables = parse(
            r#"
# a comment
API_URL=https://api.test
export TOKEN=abc123
QUOTED="hello world"
SINGLE='keeps $literal'
ESCAPED="line\none"
TRAILING=value # with a note
EMPTY=
SPACED = spaces around =
not a variable
BAD-NAME=skipped
"#,
        );
        assert_eq!(variables["API_URL"], "https://api.test");
        assert_eq!(variables["TOKEN"], "abc123", "export is allowed");
        assert_eq!(variables["QUOTED"], "hello world");
        assert_eq!(variables["SINGLE"], "keeps $literal", "single quotes are literal");
        assert_eq!(variables["ESCAPED"], "line\none");
        assert_eq!(variables["TRAILING"], "value", "a trailing comment is dropped");
        assert_eq!(variables["EMPTY"], "");
        assert_eq!(variables["SPACED"], "spaces around =", "only the first = splits");
        assert!(!variables.contains_key("BAD-NAME"));
        assert_eq!(variables.len(), 8);
    }

    #[test]
    fn lists_env_files_and_skips_examples() {
        let dir = tempfile::tempdir().unwrap();
        for name in [".env.staging", ".env", ".env.local", ".env.example", "env", "notes.txt"] {
            std::fs::write(dir.path().join(name), "A=1").unwrap();
        }
        let names: Vec<String> = files_in(dir.path()).into_iter().map(|file| file.name).collect();
        assert_eq!(
            names,
            [".env", ".env.local", ".env.staging"],
            "`.env` first, then by name"
        );
    }

    #[test]
    fn spots_the_values_worth_hiding() {
        let variables = parse(
            "API_URL=https://api.test\nAPI_TOKEN=abc\nSTRIPE_SECRET_KEY=sk_live_1\nDB_PASSWORD=hunter2\nPORT=3000",
        );
        let mut secrets = secret_names(&variables);
        secrets.sort();
        assert_eq!(secrets, ["API_TOKEN", "DB_PASSWORD", "STRIPE_SECRET_KEY"]);
    }
}
