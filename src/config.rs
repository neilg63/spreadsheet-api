use std::{path::PathBuf, sync::OnceLock};

/// Service settings, read once from the environment (and `.env`, via dotenvy).
#[derive(Debug, Clone)]
pub struct Config {
    pub address: String,
    /// Directory for the embedded SurrealKV store, or "memory" for a throwaway in-memory DB.
    pub db_path: String,
    pub db_namespace: String,
    pub db_database: String,
    /// Uploaded source files live here until they are fully imported or purged.
    pub upload_dir: PathBuf,
    pub max_upload_bytes: usize,
    /// Rows per worksheet shown in upload overviews and mapping previews.
    pub preview_rows: u32,
    /// Hard ceiling on `limit` for GET /datasets/{id}.
    pub max_page_size: u32,
    pub default_page_size: u32,
    pub sweep_interval_secs: u64,
}

impl Config {
    fn from_env() -> Self {
        let var = |key: &str, default: &str| dotenvy::var(key).unwrap_or_else(|_| default.to_string());
        let num = |key: &str, default: u64| dotenvy::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        Config {
            address: format!("{}:{}", var("LOCAL_ADDRESS", "0.0.0.0"), var("PORT", "3000")),
            db_path: var("DB_PATH", "data/db"),
            db_namespace: var("DB_NAMESPACE", "sheets"),
            db_database: var("DB_DATABASE", "api"),
            upload_dir: PathBuf::from(var("UPLOAD_DIR", "data/uploads")),
            max_upload_bytes: parse_size(&var("MAX_UPLOAD_SIZE", "50MB")).unwrap_or(50 * 1024 * 1024),
            // identity, sign-in and rate-limit settings: auth::AuthSettings
            preview_rows: num("PREVIEW_ROWS", 30) as u32,
            max_page_size: num("MAX_PAGE_SIZE", 1000) as u32,
            default_page_size: num("DEFAULT_PAGE_SIZE", 100) as u32,
            sweep_interval_secs: num("SWEEP_INTERVAL_SECS", 60),
        }
    }
}

pub fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(Config::from_env)
}

/// "50MB", "512k", "1g" or a plain byte count.
pub fn parse_size(size: &str) -> Option<usize> {
    let lower = size.trim().to_lowercase();
    let digits: String = lower.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = lower[digits.len()..].trim();
    let base: usize = digits.parse().ok()?;
    let factor = match unit.chars().next() {
        None | Some('b') => 1,
        Some('k') => 1024,
        Some('m') => 1024 * 1024,
        Some('g') => 1024 * 1024 * 1024,
        _ => return None,
    };
    Some(base * factor)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parses_sizes_with_units() {
        assert_eq!(parse_size("10k"), Some(10 * 1024));
        assert_eq!(parse_size("50MB"), Some(50 * 1024 * 1024));
        assert_eq!(parse_size("2048"), Some(2048));
        assert_eq!(parse_size("5 xb"), None);
    }
}
