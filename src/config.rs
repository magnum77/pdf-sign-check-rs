use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub webhook_path: String,
    pub debug_dir: PathBuf,
    pub log_dir: PathBuf,
    pub debug_retention_count: usize,
    pub log_retention_count: usize,
    pub temp_dir: PathBuf,
    pub max_body_bytes: usize,
    pub pdfsig_path: Option<PathBuf>,
    pub paperless: Option<PaperlessConfig>,
}

#[derive(Debug, Clone)]
pub struct PaperlessConfig {
    pub url: String,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub tag_name: String,
    pub api_version: u16,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().ok();

        let bind = env_value("PDF_SIGN_CHECK_BIND", "0.0.0.0:3000").parse()?;
        let webhook_path = normalize_path(&env_value("PDF_SIGN_CHECK_WEBHOOK_PATH", "/webhook"));
        let debug_dir = PathBuf::from(env_value("PDF_SIGN_CHECK_DEBUG_DIR", "debug"));
        let log_dir = PathBuf::from(env_value("PDF_SIGN_CHECK_LOG_DIR", "logs"));
        let debug_retention_count =
            env_value("PDF_SIGN_CHECK_DEBUG_RETENTION_COUNT", "5").parse()?;
        let log_retention_count = env_value("PDF_SIGN_CHECK_LOG_RETENTION_COUNT", "5").parse()?;
        let temp_dir = env::var_os("PDF_SIGN_CHECK_TEMP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::temp_dir().join("pdf-sign-check-rs"));
        let max_body_bytes = env_value("PDF_SIGN_CHECK_MAX_BODY_BYTES", "26214400").parse()?;
        let pdfsig_path = env::var_os("PDFSIG_PATH").map(PathBuf::from);
        let paperless = PaperlessConfig::from_env()?;

        Ok(Self {
            bind,
            webhook_path,
            debug_dir,
            log_dir,
            debug_retention_count,
            log_retention_count,
            temp_dir,
            max_body_bytes,
            pdfsig_path,
            paperless,
        })
    }

    pub fn ensure_dirs_blocking(&self) -> std::io::Result<()> {
        if self.debug_retention_count > 0 {
            ensure_dir(&self.debug_dir)?;
        }
        if self.log_retention_count > 0 {
            ensure_dir(&self.log_dir)?;
        }
        ensure_dir(&self.temp_dir)?;
        Ok(())
    }
}

impl PaperlessConfig {
    fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(url) = optional_env("PAPERLESS_URL") else {
            return Ok(None);
        };

        let token = optional_env("PAPERLESS_TOKEN");
        let username = optional_env("PAPERLESS_USERNAME");
        let password = optional_env("PAPERLESS_PASSWORD");
        let tag_name =
            optional_env("PAPERLESS_TAG_NAME").unwrap_or_else(|| "Podpisany cyfrowo".to_owned());
        let api_version = env_value("PAPERLESS_API_VERSION", "9").parse()?;

        Ok(Some(Self {
            url: normalize_url(&url),
            token,
            username,
            password,
            tag_name,
            api_version,
        }))
    }
}

fn env_value(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn optional_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/webhook".to_owned();
    }

    if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    }
}

fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}
