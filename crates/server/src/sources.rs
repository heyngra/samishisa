use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    config::FullEntry,
    log::{self, Level},
    netguard::{Egress, OutRequest, Policy},
};

pub static BUILTIN_HELLO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.wasm"));

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    Crate(PathBuf),
    Builtin(String),
    GitHub { owner: String, repo: String, tag: Option<String> },
    Url(String),
}

pub const FORMS: &str = "a .wasm file (./hello.wasm), an extension crate folder (./my-extension), builtin:hello, github:owner/repo[#tag], or an https:// URL of a .wasm file";

pub fn parse(src: &str, resolve_path: impl Fn(&str) -> PathBuf) -> anyhow::Result<Source> {
    let s = src.trim();
    if let Some(name) = s.strip_prefix("builtin:") {
        return Ok(Source::Builtin(name.to_owned()));
    }
    if let Some(rest) = s.strip_prefix("github:") {
        let (repo_part, tag) = match rest.split_once('#') {
            Some((r, t)) if !t.is_empty() => (r, Some(t.to_owned())),
            _ => (rest, None),
        };
        let (owner, repo) = repo_part.split_once('/').context("github sources look like github:owner/repo or github:owner/repo#tag")?;
        let ok = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !ok(owner) || !ok(repo) {
            bail!("{src:?} is not a valid github:owner/repo source");
        }
        return Ok(Source::GitHub { owner: owner.into(), repo: repo.trim_end_matches(".git").into(), tag });
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        return Ok(Source::Url(s.to_owned()));
    }
    if s.contains("://") || s.starts_with("git@") {
        bail!("{src:?}: git repositories can't be used directly (extensions are compiled). Publish a release with the .wasm and use github:owner/repo, or a URL of the .wasm file.\nSupported: {FORMS}");
    }
    let path = resolve_path(s);
    if path.is_dir() {
        if path.join("Cargo.toml").exists() {
            return Ok(Source::Crate(path));
        }
        bail!("{} is a folder without a Cargo.toml. Supported: {FORMS}", path.display());
    }
    Ok(Source::File(path))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Lock {
    #[serde(default = "lock_version")]
    pub version: u32,
    #[serde(default)]
    pub sources: BTreeMap<String, Locked>,
}

fn lock_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Locked {
    pub url: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

impl Lock {
    pub fn read(path: &Path) -> anyhow::Result<Lock> {
        if !path.exists() {
            return Ok(Lock { version: 1, ..Default::default() });
        }
        serde_json::from_str(&std::fs::read_to_string(path)?).with_context(|| format!("{} is not a valid lockfile (delete it to re-resolve every source)", path.display()))
    }
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub struct Resolver<'a> {
    pub data_dir: &'a Path,
    pub lock: &'a mut Lock,
    pub update: bool,
    pub downloader: &'a Egress,
}

impl Resolver<'_> {
    pub async fn resolve(&mut self, entry: &FullEntry, resolve_path: impl Fn(&str) -> PathBuf) -> anyhow::Result<PathBuf> {
        let source = parse(&entry.source, resolve_path)?;
        let path = match &source {
            Source::File(p) => {
                if !p.exists() {
                    bail!("{} not found", p.display());
                }
                p.clone()
            }
            Source::Crate(dir) => {
                log::write(Level::Info, "core", &format!("building {} (a crate folder source)", dir.display()));
                crate::scaffold::build(dir).with_context(|| format!("building {}", dir.display()))?
            }
            Source::Builtin(name) => {
                if name != "hello" {
                    bail!("there is no builtin:{name} (available: builtin:hello)");
                }
                if BUILTIN_HELLO.is_empty() {
                    bail!("this samishisa build doesn't include builtin:hello; use github:heyngra/samishisa (the release's hello.wasm) instead");
                }
                let p = self.data_dir.join("builtin").join(format!("hello-{}.wasm", &sha256_hex(BUILTIN_HELLO)[..12]));
                if !p.exists() {
                    std::fs::create_dir_all(p.parent().expect("has parent"))?;
                    std::fs::write(&p, BUILTIN_HELLO)?;
                }
                p
            }
            Source::GitHub { .. } | Source::Url(_) => self.remote(&entry.source, &source, entry.asset.as_deref()).await?,
        };
        if let Some(want) = &entry.sha256 {
            let got = sha256_hex(&std::fs::read(&path)?);
            if !got.eq_ignore_ascii_case(want.trim()) {
                bail!("{} has SHA-256 {got}, but the config requires {want}", entry.source);
            }
        }
        Ok(path)
    }

    async fn remote(&mut self, key: &str, source: &Source, asset: Option<&str>) -> anyhow::Result<PathBuf> {
        let cache_dir = self.data_dir.join("sources");
        if !self.update {
            if let Some(pin) = self.lock.sources.get(key).cloned() {
                let p = cache_dir.join(format!("{}.wasm", pin.sha256));
                if p.exists() {
                    return Ok(p);
                }
                let bytes = self.download(&pin.url).await?;
                let got = sha256_hex(&bytes);
                if got != pin.sha256 {
                    bail!("{key}: {} now has SHA-256 {got}, but api.lock.json pins {}. If the change is expected, run `samishisa update`.", pin.url, pin.sha256);
                }
                return self.save(&cache_dir, &bytes, &got);
            }
        }
        let (url, resolved) = match source {
            Source::Url(u) => (u.clone(), None),
            Source::GitHub { owner, repo, tag } => self.github_asset(owner, repo, tag.as_deref(), asset).await?,
            _ => unreachable!("only remote sources get here"),
        };
        let bytes = self.download(&url).await?;
        if bytes.get(..4) != Some(b"\0asm") {
            bail!("{url} is not a WebAssembly file");
        }
        let sha = sha256_hex(&bytes);
        let before = self.lock.sources.get(key).map(|l| l.sha256.clone());
        self.lock.sources.insert(key.to_owned(), Locked { url: url.clone(), sha256: sha.clone(), resolved: resolved.clone() });
        match before {
            Some(old) if old != sha => log::write(Level::Info, "core", &format!("{key}: updated to {} ({})", resolved.as_deref().unwrap_or(&url), &sha[..12])),
            None => log::write(Level::Info, "core", &format!("{key}: pinned {} ({})", resolved.as_deref().unwrap_or(&url), &sha[..12])),
            _ => {}
        }
        self.save(&cache_dir, &bytes, &sha)
    }

    fn save(&self, dir: &Path, bytes: &[u8], sha: &str) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let p = dir.join(format!("{sha}.wasm"));
        let tmp = p.with_extension("part");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &p)?;
        Ok(p)
    }

    async fn get(&self, url: &str, accept: &str) -> anyhow::Result<Vec<u8>> {
        let mut headers = vec![("accept".to_owned(), accept.to_owned())];
        if url.starts_with("https://api.github.com/") {
            if let Ok(t) = std::env::var("GITHUB_TOKEN") {
                headers.push(("authorization".into(), format!("Bearer {t}")));
            }
        }
        let res = self
            .downloader
            .fetch("samishisa", &Policy::Any, OutRequest { method: "GET".into(), url: url.to_owned(), headers, body: None })
            .await
            .map_err(|e| anyhow::anyhow!("{url}: {}{}", e.message, e.details_json.map(|d| format!(" {d}")).unwrap_or_default()))?;
        if res.status != 200 {
            bail!("{url} answered {}: {}", res.status, String::from_utf8_lossy(&res.body[..res.body.len().min(200)]));
        }
        Ok(res.body)
    }

    async fn download(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        self.get(url, "application/octet-stream").await
    }

    async fn github_asset(&self, owner: &str, repo: &str, tag: Option<&str>, asset: Option<&str>) -> anyhow::Result<(String, Option<String>)> {
        let api = match tag {
            Some(t) => format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{t}"),
            None => format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
        };
        let release: Value = serde_json::from_slice(&self.get(&api, "application/vnd.github+json").await.with_context(|| {
            format!("finding the {} release of github:{owner}/{repo} (does it have a published release?)", tag.unwrap_or("latest"))
        })?)?;
        let tag_name = release["tag_name"].as_str().map(str::to_owned);
        let assets: Vec<(&str, &str)> = release["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| Some((a["name"].as_str()?, a["browser_download_url"].as_str()?)))
            .filter(|(n, _)| n.ends_with(".wasm"))
            .collect();
        let pick = match asset {
            Some(want) => assets.iter().find(|(n, _)| *n == want),
            None if assets.len() == 1 => assets.first(),
            None => assets.iter().find(|(n, _)| n.trim_end_matches(".wasm") == repo || n.trim_end_matches(".wasm").replace('_', "-") == repo),
        };
        let names: Vec<&str> = assets.iter().map(|(n, _)| *n).collect();
        let (_, url) = pick.with_context(|| match (asset, names.is_empty()) {
            (_, true) => format!("github:{owner}/{repo} release {} has no .wasm asset", tag_name.as_deref().unwrap_or("?")),
            (Some(a), _) => format!("github:{owner}/{repo} has no asset {a:?} (it has: {})", names.join(", ")),
            (None, _) => format!("github:{owner}/{repo} has several .wasm assets ({}); choose one with \"asset\" in the config entry", names.join(", ")),
        })?;
        Ok((url.to_string(), tag_name.map(|t| format!("{owner}/{repo}@{t}"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources() {
        let r = |s: &str| PathBuf::from("/base").join(s);
        assert_eq!(parse("builtin:hello", r).unwrap(), Source::Builtin("hello".into()));
        assert_eq!(parse("github:o/r#v1.2", r).unwrap(), Source::GitHub { owner: "o".into(), repo: "r".into(), tag: Some("v1.2".into()) });
        assert_eq!(parse("github:o/r", r).unwrap(), Source::GitHub { owner: "o".into(), repo: "r".into(), tag: None });
        assert_eq!(parse("https://x.example/e.wasm", r).unwrap(), Source::Url("https://x.example/e.wasm".into()));
        assert_eq!(parse("./e.wasm", r).unwrap(), Source::File(PathBuf::from("/base/./e.wasm")));
        assert!(parse("github:nope", r).is_err());
        assert!(parse("git@github.com:o/r.git", r).unwrap_err().to_string().contains("release"));
    }
}
