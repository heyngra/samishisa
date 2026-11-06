use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::Value;

use crate::{
    config::{Config, interpolate_env, local_address},
    log::{log_debug, log_error, log_info, log_warn},
};

pub struct Tunnel {
    child: Arc<Mutex<Child>>,
    stopping: Arc<AtomicBool>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub fn start(cfg: &Config) -> Option<Tunnel> {
    let tunnel = &cfg.tunnel;
    if !tunnel.enabled {
        return None;
    }
    let binary = tunnel.binary.clone().unwrap_or_else(|| "cloudflared".into());
    let token = match &tunnel.token {
        Some(token) => match interpolate_env(&Value::String(token.clone()), "tunnel.token") {
            Ok(Value::String(token)) if !token.trim().is_empty() => Some(token),
            Ok(_) => None,
            Err(e) => {
                log_error!("tunnel", "not started: {e:#}");
                return None;
            }
        },
        None => None,
    };
    let target = local_address(&cfg.host, cfg.port);
    let mut command = Command::new(&binary);
    command.arg("tunnel").arg("--no-autoupdate");
    match &token {
        Some(token) => {
            command.args(["run"]).env("TUNNEL_TOKEN", token);
        }
        None => {
            command.arg("--url").arg(format!("http://{target}"));
        }
    }
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_error!(
                "tunnel",
                "`{binary}` not found. Install cloudflared (Windows: `winget install Cloudflare.cloudflared`; macOS: `brew install cloudflared`; Linux: https://pkg.cloudflare.com), or set tunnel.binary"
            );
            return None;
        }
        Err(e) => {
            log_error!("tunnel", "could not start `{binary}`: {e}");
            return None;
        }
    };
    log_info!("tunnel", "starting {} tunnel ({binary})", if token.is_some() { "named" } else { "quick" });
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    let stopping = Arc::new(AtomicBool::new(false));
    if let Some(stderr) = stderr {
        let public_url = tunnel.url.clone();
        let (child, stopping) = (child.clone(), stopping.clone());
        std::thread::spawn(move || {
            let mut announced = false;
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(url) = quick_tunnel_url(&line) {
                    log_info!("tunnel", "public address: {url}  (new on every start; use a named tunnel for a fixed one)");
                } else if !announced && line.contains("Registered tunnel connection") {
                    announced = true;
                    match &public_url {
                        Some(url) => log_info!("tunnel", "connected: {url}"),
                        None => log_info!("tunnel", "connected"),
                    }
                } else if line.contains(" ERR ") {
                    log_warn!("tunnel", "{}", line.trim());
                } else {
                    log_debug!("tunnel", "{}", line.trim());
                }
            }
            if !stopping.load(Ordering::SeqCst) {
                let status = child.lock().unwrap_or_else(|e| e.into_inner()).wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                log_warn!("tunnel", "cloudflared stopped ({status}): the server is no longer reachable through the tunnel. Restart samishisa to try again.");
            }
        });
    }
    Some(Tunnel { child, stopping })
}

fn quick_tunnel_url(line: &str) -> Option<&str> {
    let start = line.find("https://")?;
    let url = line[start..].split_whitespace().next()?;
    let url = url.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    url.contains(".trycloudflare.com").then_some(url)
}

#[cfg(test)]
mod tests {
    use super::quick_tunnel_url;

    #[test]
    fn finds_the_quick_tunnel_address() {
        let line = "2026-11-06T18:00:00Z INF |  https://quiet-lake-example.trycloudflare.com                                  |";
        assert_eq!(quick_tunnel_url(line), Some("https://quiet-lake-example.trycloudflare.com"));
        assert_eq!(quick_tunnel_url("INF Requesting new quick Tunnel on trycloudflare.com..."), None);
        assert_eq!(quick_tunnel_url("ERR failed https://api.cloudflare.com/x"), None);
    }
}
