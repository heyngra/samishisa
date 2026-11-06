use std::time::Duration;

use anyhow::Context;

use super::{describe::usage, input::Validators};
use crate::{
    cache::CacheRule,
    manifest::RouteManifest,
    ratelimit::{RateSettings, RouteLimit},
};

enum Segment {
    Literal(String),
    Param(String),
}

pub struct Route {
    pub index: u32,
    pub spec: RouteManifest,
    segments: Vec<Segment>,
    literal_count: usize,
    pub usage: String,
    pub timeout: Duration,
    pub validators: Validators,
    pub rate_limit: Option<RouteLimit>,
    pub cache: Option<CacheRule>,
}

pub fn split_path(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

impl Route {
    pub fn compile(mount: &str, index: usize, spec: &RouteManifest, default_timeout: Duration, rate_settings: RateSettings) -> anyhow::Result<Route> {
        let segments: Vec<Segment> = split_path(&spec.path)
            .into_iter()
            .map(|s| match s.strip_prefix(':') {
                Some(name) => Segment::Param(name.to_owned()),
                None => Segment::Literal(s.to_owned()),
            })
            .collect();
        Ok(Route {
            index: u32::try_from(index).context("too many routes")?,
            literal_count: segments.iter().filter(|s| matches!(s, Segment::Literal(_))).count(),
            segments,
            usage: usage(mount, spec),
            timeout: spec.timeout_ms.map(Duration::from_millis).unwrap_or(default_timeout),
            validators: Validators::compile(spec)?,
            rate_limit: RouteLimit::resolve(rate_settings, mount, index),
            cache: spec.cache.filter(|c| c.fresh_secs > 0 && spec.method == "GET"),
            spec: spec.clone(),
        })
    }

    fn match_path<'p>(&self, parts: &[&'p str]) -> Option<Vec<(&str, &'p str)>> {
        if parts.len() != self.segments.len() {
            return None;
        }
        let mut params = Vec::new();
        for (segment, part) in self.segments.iter().zip(parts) {
            match segment {
                Segment::Literal(literal) if literal == part => {}
                Segment::Literal(_) => return None,
                Segment::Param(name) => params.push((name.as_str(), *part)),
            }
        }
        Some(params)
    }
}

pub enum Match<'r, 'p> {
    Found { route: &'r Route, params: Vec<(&'r str, &'p str)> },
    WrongMethod { allowed: Vec<&'r str> },
    NotFound,
}

pub fn find<'r, 'p>(routes: &'r [Route], method: &str, parts: &[&'p str]) -> Match<'r, 'p> {
    let mut best: Option<(&Route, Vec<(&str, &str)>)> = None;
    let mut allowed: Vec<&str> = Vec::new();
    for route in routes {
        let Some(params) = route.match_path(parts) else { continue };
        if route.spec.method != method {
            allowed.push(&route.spec.method);
        } else if best.as_ref().is_none_or(|(b, _)| route.literal_count > b.literal_count) {
            best = Some((route, params));
        }
    }
    match best {
        Some((route, params)) => Match::Found { route, params },
        None if !allowed.is_empty() => {
            allowed.sort();
            allowed.dedup();
            Match::WrongMethod { allowed }
        }
        None => Match::NotFound,
    }
}

pub fn closest<'a>(word: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    fn edit_distance(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut previous: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut current = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                current.push((previous[j] + usize::from(ca != *cb)).min(previous[j + 1] + 1).min(current[j] + 1));
            }
            previous = current;
        }
        previous[b.len()]
    }
    candidates.map(|c| (edit_distance(word, c), c)).filter(|(distance, c)| *distance <= 2.max(c.len() / 3)).min_by_key(|(distance, _)| *distance).map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions() {
        assert_eq!(closest("helo", ["hello", "weather"].into_iter()), Some("hello"));
        assert_eq!(closest("zzzzzz", ["hello"].into_iter()), None);
    }
}
