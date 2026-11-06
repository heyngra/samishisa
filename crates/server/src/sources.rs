use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    config::{Config, ExtensionEntry},
    log::{log_error, log_info, log_warn},
    netguard::{Egress, OutRequest, Policy},
};

static BUILTIN_HELLO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.wasm"));

pub fn has_builtin_hello() -> bool {
    !BUILTIN_HELLO.is_empty()
}

pub const SERVER_BUILTINS: &[&str] = &["docs"];

pub fn is_server_builtin(source: &str) -> bool {
    source.trim().strip_prefix("builtin:").is_some_and(|name| SERVER_BUILTINS.contains(&name))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    Crate(PathBuf),
    Builtin(String),
    Remote(Remote),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Remote {
    Url(String),
    GitHub { owner: String, repo: String, tag: Option<String> },
}

const FORMS: &str = "a .wasm file (./hello.wasm), an extension crate folder (./my-extension), builtin:hello, github:owner/repo[#tag], or an https:// URL of a .wasm file";

pub fn parse(source: &str, base_dir: &Path) -> anyhow::Result<Source> {
    let s = source.trim();
    if let Some(name) = s.strip_prefix("builtin:") {
        if name != "hello" && !SERVER_BUILTINS.contains(&name) {
            bail!("there is no builtin:{name} (available: builtin:hello, builtin:docs)");
        }
        return Ok(Source::Builtin(name.to_owned()));
    }
    if let Some(rest) = s.strip_prefix("github:") {
        let (repo_part, tag) = match rest.split_once('#') {
            Some((r, t)) if !t.is_empty() => (r, Some(t.to_owned())),
            _ => (rest, None),
        };
        let (owner, repo) = repo_part.split_once('/').context("github sources look like github:owner/repo or github:owner/repo#tag")?;
        let valid = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !valid(owner) || !valid(repo) {
            bail!("{source:?} is not a valid github:owner/repo source");
        }
        return Ok(Source::Remote(Remote::GitHub { owner: owner.into(), repo: repo.trim_end_matches(".git").into(), tag }));
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        return Ok(Source::Remote(Remote::Url(s.to_owned())));
    }
    if s.contains("://") || s.starts_with("git@") {
        bail!(
            "{source:?}: git repositories can't be used directly (extensions are compiled). Publish a release with the .wasm and use github:owner/repo, or a URL of the .wasm file.\nSupported: {FORMS}"
        );
    }
    let path = local_path(base_dir, s);
    if path.is_dir() {
        if path.join("Cargo.toml").exists() {
            return Ok(Source::Crate(path));
        }
        bail!("{} is a folder without a Cargo.toml. Supported: {FORMS}", path.display());
    }
    Ok(Source::File(path))
}

fn local_path(base_dir: &Path, path: &str) -> PathBuf {
    let path = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    };
    if path.is_absolute() { path } else { base_dir.join(path) }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct Lock {
    version: u32,
    sources: BTreeMap<String, Pin>,
}

impl Default for Lock {
    fn default() -> Self {
        Lock { version: 1, sources: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Pin {
    url: String,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved: Option<String>,
}

pub fn lock_path(cfg: &Config) -> PathBuf {
    cfg.base_dir.join("api.lock.json")
}

impl Lock {
    fn read(path: &Path) -> anyhow::Result<Lock> {
        if !path.exists() {
            return Ok(Lock::default());
        }
        serde_json::from_str(&std::fs::read_to_string(path)?).with_context(|| format!("{} is not a valid lockfile (delete it to re-resolve every source)", path.display()))
    }

    fn write(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pins {
    Keep,
    Update,
}

pub async fn resolve_all(cfg: &Config, downloader: &Egress, pins: Pins) -> Vec<(ExtensionEntry, anyhow::Result<PathBuf>)> {
    let lock_file = lock_path(cfg);
    let mut lock = Lock::read(&lock_file).unwrap_or_else(|e| {
        log_error!("core", "{e:#}");
        Lock::default()
    });
    let before = lock.clone();
    let mut resolver = Resolver { base_dir: &cfg.base_dir, data_dir: &cfg.data_dir, lock: &mut lock, pins, downloader };
    let mut resolved = Vec::new();
    for entry in cfg.extensions.iter().filter(|e| !is_server_builtin(&e.source)) {
        resolved.push((entry.clone(), resolver.resolve(entry).await));
    }
    if lock != before
        && let Err(e) = lock.write(&lock_file)
    {
        log_warn!("core", "could not save {} ({e:#}): extensions loaded, but their versions are not pinned", lock_file.display());
    }
    resolved
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct Resolver<'a> {
    base_dir: &'a Path,
    data_dir: &'a Path,
    lock: &'a mut Lock,
    pins: Pins,
    downloader: &'a Egress,
}

impl Resolver<'_> {
    async fn resolve(&mut self, entry: &ExtensionEntry) -> anyhow::Result<PathBuf> {
        let path = match parse(&entry.source, self.base_dir)? {
            Source::File(p) if p.exists() => p,
            Source::File(p) => bail!("{} not found", p.display()),
            Source::Crate(dir) => {
                log_info!("core", "building {} (a crate folder source)", dir.display());
                let crate_dir = dir.clone();
                tokio::task::spawn_blocking(move || crate::scaffold::build(&crate_dir))
                    .await
                    .context("the build panicked")?
                    .with_context(|| format!("building {}", dir.display()))?
            }
            Source::Builtin(name) => self.builtin(&name)?,
            Source::Remote(remote) => self.remote(&entry.source, &remote, entry.asset.as_deref()).await?,
        };
        if let Some(want) = &entry.sha256 {
            let got = sha256_hex(&std::fs::read(&path)?);
            if !got.eq_ignore_ascii_case(want.trim()) {
                bail!("{} has SHA-256 {got}, but the config requires {want}", entry.source);
            }
        }
        Ok(path)
    }

    fn builtin(&self, name: &str) -> anyhow::Result<PathBuf> {
        if name != "hello" {
            bail!("there is no builtin:{name} (available: builtin:hello, builtin:docs)");
        }
        if !has_builtin_hello() {
            bail!("this samishisa build doesn't include builtin:hello; use github:heyngra/samishisa (the release's hello.wasm) instead");
        }
        let dir = self.data_dir.join("builtin");
        let path = dir.join(format!("hello-{}.wasm", &sha256_hex(BUILTIN_HELLO)[..12]));
        if !path.exists() {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&path, BUILTIN_HELLO)?;
        }
        Ok(path)
    }

    async fn remote(&mut self, key: &str, remote: &Remote, asset: Option<&str>) -> anyhow::Result<PathBuf> {
        let downloads = self.data_dir.join("sources");
        if self.pins == Pins::Keep
            && let Some(pin) = self.lock.sources.get(key).cloned()
        {
            let cached = downloads.join(format!("{}.wasm", pin.sha256));
            if cached.exists() {
                return Ok(cached);
            }
            let bytes = self.download(&pin.url).await?;
            let got = sha256_hex(&bytes);
            if got != pin.sha256 {
                bail!("{key}: {} now has SHA-256 {got}, but api.lock.json pins {}. If the change is expected, run `samishisa update`.", pin.url, pin.sha256);
            }
            return save(&downloads, &bytes, &got);
        }
        let (url, resolved) = match remote {
            Remote::Url(url) => (url.clone(), None),
            Remote::GitHub { owner, repo, tag } => self.github_asset(owner, repo, tag.as_deref(), asset).await?,
        };
        let bytes = self.download(&url).await?;
        if bytes.get(..4) != Some(b"\0asm") {
            bail!("{url} is not a WebAssembly file");
        }
        let sha = sha256_hex(&bytes);
        let shown = format!("{} ({})", resolved.as_deref().unwrap_or(&url), &sha[..12]);
        match self.lock.sources.insert(key.to_owned(), Pin { url, sha256: sha.clone(), resolved }) {
            Some(old) if old.sha256 != sha => log_info!("core", "{key}: updated to {shown}"),
            None => log_info!("core", "{key}: pinned {shown}"),
            Some(_) => {}
        }
        save(&downloads, &bytes, &sha)
    }

    async fn download(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        self.get(url, "application/octet-stream").await
    }

    async fn get(&self, url: &str, accept: &str) -> anyhow::Result<Vec<u8>> {
        let mut headers = vec![("accept".to_owned(), accept.to_owned())];
        if url.starts_with("https://api.github.com/")
            && let Ok(token) = std::env::var("GITHUB_TOKEN")
        {
            headers.push(("authorization".into(), format!("Bearer {token}")));
        }
        let request = OutRequest { method: "GET".into(), url: url.to_owned(), headers, body: None };
        let response = self.downloader.fetch("samishisa", &Policy::Any, request).await.map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
        if response.status != 200 {
            bail!("{url} answered {}: {}", response.status, String::from_utf8_lossy(&response.body[..response.body.len().min(200)]));
        }
        Ok(response.body)
    }

    async fn github_asset(&self, owner: &str, repo: &str, tag: Option<&str>, asset: Option<&str>) -> anyhow::Result<(String, Option<String>)> {
        let api = match tag {
            Some(t) => format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{t}"),
            None => format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
        };
        let body = self
            .get(&api, "application/vnd.github+json")
            .await
            .with_context(|| format!("finding the {} release of github:{owner}/{repo} (does it have a published release?)", tag.unwrap_or("latest")))?;
        let release: Value = serde_json::from_slice(&body)?;
        let tag_name = release["tag_name"].as_str().unwrap_or("?").to_owned();
        let wasm_assets: Vec<(&str, &str)> = release["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| Some((a["name"].as_str()?, a["browser_download_url"].as_str()?)))
            .filter(|(name, _)| name.ends_with(".wasm"))
            .collect();
        let names: Vec<&str> = wasm_assets.iter().map(|(name, _)| *name).collect();
        let named_like_repo = |name: &str| {
            let stem = name.trim_end_matches(".wasm");
            stem == repo || stem.replace('_', "-") == repo
        };
        let chosen = match asset {
            Some(wanted) => wasm_assets.iter().find(|(name, _)| *name == wanted),
            None if wasm_assets.len() == 1 => wasm_assets.first(),
            None => wasm_assets.iter().find(|(name, _)| named_like_repo(name)),
        };
        let Some((_, url)) = chosen else {
            match (asset, names.is_empty()) {
                (_, true) => bail!("github:{owner}/{repo} release {tag_name} has no .wasm asset"),
                (Some(a), _) => bail!("github:{owner}/{repo} has no asset {a:?} (it has: {})", names.join(", ")),
                (None, _) => bail!("github:{owner}/{repo} has several .wasm assets ({}); choose one with \"asset\" in the config entry", names.join(", ")),
            }
        };
        Ok((url.to_string(), Some(format!("{owner}/{repo}@{tag_name}"))))
    }
}

fn save(dir: &Path, bytes: &[u8], sha: &str) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{sha}.wasm"));
    let partial = path.with_extension("part");
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources() {
        let base = Path::new("/base");
        let github = |owner: &str, repo: &str, tag: Option<&str>| Source::Remote(Remote::GitHub { owner: owner.into(), repo: repo.into(), tag: tag.map(Into::into) });
        assert_eq!(parse("builtin:hello", base).unwrap(), Source::Builtin("hello".into()));
        assert_eq!(parse("builtin:docs", base).unwrap(), Source::Builtin("docs".into()));
        assert!(parse("builtin:nope", base).is_err());
        assert_eq!(parse("github:o/r#v1.2", base).unwrap(), github("o", "r", Some("v1.2")));
        assert_eq!(parse("github:o/r", base).unwrap(), github("o", "r", None));
        assert_eq!(parse("https://x.example/e.wasm", base).unwrap(), Source::Remote(Remote::Url("https://x.example/e.wasm".into())));
        assert_eq!(parse("./e.wasm", base).unwrap(), Source::File(PathBuf::from("/base/./e.wasm")));
        assert!(parse("github:nope", base).is_err());
        assert!(parse("git@github.com:o/r.git", base).unwrap_err().to_string().contains("release"));
    }
}
