use std::io::Write;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    HeaderMap, Response,
    header::{CONTENT_ENCODING, HeaderValue, VARY},
};

const MIN_BYTES: usize = 1024;

pub fn accepts_gzip(request_headers: &HeaderMap) -> bool {
    request_headers.get("accept-encoding").and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|encoding| encoding.trim().starts_with("gzip")))
}

fn is_compressible(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") || ct.contains("javascript")
}

pub fn gzip_if_worthwhile(content_type: Option<&str>, body: &[u8]) -> Option<Bytes> {
    if body.len() < MIN_BYTES || !content_type.is_some_and(is_compressible) {
        return None;
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 3), flate2::Compression::new(6));
    encoder.write_all(body).ok()?;
    let gz = encoder.finish().ok()?;
    (gz.len() < body.len()).then(|| Bytes::from(gz))
}

#[derive(Clone)]
pub struct Precompressed(pub Bytes);

pub async fn compress(response: Response<Full<Bytes>>) -> Response<Full<Bytes>> {
    if response.headers().contains_key("content-encoding") || matches!(response.status().as_u16(), 204 | 304) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let raw = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(never) => match never {},
    };
    let gz = match parts.extensions.remove::<Precompressed>() {
        Some(Precompressed(gz)) => Some(gz),
        None => gzip_if_worthwhile(parts.headers.get("content-type").and_then(|v| v.to_str().ok()), &raw),
    };
    let Some(gz) = gz else {
        return Response::from_parts(parts, Full::new(raw));
    };
    parts.headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    parts.headers.append(VARY, HeaderValue::from_static("accept-encoding"));
    parts.headers.remove("content-length");
    Response::from_parts(parts, Full::new(gz))
}
