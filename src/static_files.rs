use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use cloudflare_pingora::http::ResponseHeader;
use cloudflare_pingora::proxy::Session;
use cloudflare_pingora::Result;
use http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
    IF_NONE_MATCH, LAST_MODIFIED, VARY,
};
use http::Method;
use lru::LruCache;
use parking_lot::Mutex;
use percent_encoding::percent_decode_str;
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

const MAX_BUFFERED_ASSET_BYTES: u64 = 8 * 1024 * 1024;
const FILE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Encoding {
    Identity,
    Gzip,
    Brotli,
    Zstd,
}

impl Encoding {
    fn header(self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Gzip => Some("gzip"),
            Self::Brotli => Some("br"),
            Self::Zstd => Some("zstd"),
        }
    }

    fn extension(self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Gzip => Some("gz"),
            Self::Brotli => Some("br"),
            Self::Zstd => Some("zst"),
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct CacheKey {
    path: PathBuf,
    modified_nanos: u128,
    length: u64,
    encoding: Encoding,
}

struct CachedAsset {
    body: Bytes,
    content_type: String,
    etag: String,
    last_modified: String,
}

struct CacheState {
    assets: LruCache<CacheKey, Arc<CachedAsset>>,
    bytes: usize,
}

struct AssetCache {
    max_bytes: usize,
    state: Mutex<CacheState>,
}

impl AssetCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            state: Mutex::new(CacheState {
                assets: LruCache::new(NonZeroUsize::new(512).unwrap()),
                bytes: 0,
            }),
        }
    }

    fn get(&self, key: &CacheKey) -> Option<Arc<CachedAsset>> {
        self.state.lock().assets.get(key).cloned()
    }

    fn insert(&self, key: CacheKey, asset: Arc<CachedAsset>) {
        if asset.body.len() > self.max_bytes {
            return;
        }

        let mut state = self.state.lock();
        if let Some(previous) = state.assets.put(key, asset.clone()) {
            state.bytes = state.bytes.saturating_sub(previous.body.len());
        }
        state.bytes += asset.body.len();
        while state.bytes > self.max_bytes {
            let Some((_, evicted)) = state.assets.pop_lru() else {
                break;
            };
            state.bytes = state.bytes.saturating_sub(evicted.body.len());
        }
    }
}

pub struct StaticFiles {
    roots: HashMap<String, PathBuf>,
    cache: AssetCache,
    compression_slot: Semaphore,
}

impl StaticFiles {
    pub fn new(roots: HashMap<String, PathBuf>, cache_bytes: usize) -> anyhow::Result<Self> {
        let mut canonical_roots = HashMap::with_capacity(roots.len());
        for (name, root) in roots {
            let canonical = std::fs::canonicalize(&root).map_err(|error| {
                anyhow::anyhow!(
                    "failed to resolve static root {} for {name}: {error}",
                    root.display()
                )
            })?;
            canonical_roots.insert(name, canonical);
        }
        Ok(Self {
            roots: canonical_roots,
            cache: AssetCache::new(cache_bytes),
            // One compressor keeps cold-cache bursts bounded on a 1 vCPU host.
            compression_slot: Semaphore::new(1),
        })
    }

    pub async fn serve(&self, host_name: &str, session: &mut Session, tls: bool) -> Result<bool> {
        let method = session.req_header().method.clone();
        if method != Method::GET && method != Method::HEAD {
            return send_empty(session, 405, &[("allow", "GET, HEAD")], tls).await;
        }

        let Some(root) = self.roots.get(host_name) else {
            return send_empty(session, 500, &[], tls).await;
        };
        let uri_path = session.req_header().uri.path();
        let Some((path, spa_fallback)) = resolve_path(root, uri_path).await else {
            return send_empty(session, 404, &[], tls).await;
        };

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return send_empty(session, 404, &[], tls).await,
        };
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let modified_nanos = modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path_is_compressible = compressible_path(&path);
        if metadata.len() > MAX_BUFFERED_ASSET_BYTES {
            let content_type = content_type(&path);
            return serve_streaming_file(
                session,
                &path,
                &metadata,
                &content_type,
                &method,
                spa_fallback,
                tls,
            )
            .await;
        }
        let requested_encoding = if metadata.len() >= 1024 && path_is_compressible {
            negotiate_encoding(
                session
                    .req_header()
                    .headers
                    .get(ACCEPT_ENCODING)
                    .and_then(|value| value.to_str().ok()),
            )
        } else {
            Encoding::Identity
        };
        let key = CacheKey {
            path: path.clone(),
            modified_nanos,
            length: metadata.len(),
            encoding: requested_encoding,
        };

        let asset = if let Some(asset) = self.cache.get(&key) {
            asset
        } else {
            let body = self.read_representation(&path, requested_encoding).await;
            let (body, actual_encoding) = match body {
                Ok(value) => value,
                Err(_) => return send_empty(session, 500, &[], tls).await,
            };
            let etag = format!("W/\"{:x}-{:x}\"", metadata.len(), modified_nanos);
            let asset = Arc::new(CachedAsset {
                body,
                content_type: content_type(&path),
                etag,
                last_modified: httpdate::fmt_http_date(modified),
            });
            let actual_key = CacheKey {
                encoding: actual_encoding,
                ..key
            };
            self.cache.insert(actual_key, asset.clone());
            asset
        };

        if session
            .req_header()
            .headers
            .get(IF_NONE_MATCH)
            .is_some_and(|value| value.as_bytes() == asset.etag.as_bytes())
        {
            let mut response = ResponseHeader::build(304, Some(8)).unwrap();
            response.insert_header(ETAG, asset.etag.as_str())?;
            response.insert_header(LAST_MODIFIED, asset.last_modified.as_str())?;
            response.insert_header(VARY, "Accept-Encoding")?;
            insert_cache_header(&mut response, &path, spa_fallback)?;
            insert_security_headers(&mut response, tls)?;
            session
                .write_response_header(Box::new(response), true)
                .await?;
            return Ok(true);
        }

        let mut response = ResponseHeader::build(200, Some(12)).unwrap();
        response.insert_header(CONTENT_TYPE, asset.content_type.as_str())?;
        response.insert_header(CONTENT_LENGTH, asset.body.len().to_string())?;
        response.insert_header(ETAG, asset.etag.as_str())?;
        response.insert_header(LAST_MODIFIED, asset.last_modified.as_str())?;
        response.insert_header(VARY, "Accept-Encoding")?;
        if let Some(encoding) = requested_encoding.header() {
            response.insert_header(CONTENT_ENCODING, encoding)?;
        }
        insert_cache_header(&mut response, &path, spa_fallback)?;
        insert_security_headers(&mut response, tls)?;

        let head = method == Method::HEAD;
        session
            .write_response_header(Box::new(response), head || asset.body.is_empty())
            .await?;
        if !head && !asset.body.is_empty() {
            session
                .write_response_body(Some(asset.body.clone()), true)
                .await?;
        }
        Ok(true)
    }

    async fn read_representation(
        &self,
        path: &Path,
        encoding: Encoding,
    ) -> std::io::Result<(Bytes, Encoding)> {
        if let Some(extension) = encoding.extension() {
            let mut sidecar: OsString = path.as_os_str().to_owned();
            sidecar.push(".");
            sidecar.push(extension);
            if let Ok(data) = tokio::fs::read(PathBuf::from(sidecar)).await {
                return Ok((Bytes::from(data), encoding));
            }
        }

        if encoding == Encoding::Identity {
            return tokio::fs::read(path)
                .await
                .map(|data| (Bytes::from(data), Encoding::Identity));
        }

        let _permit = self
            .compression_slot
            .acquire()
            .await
            .map_err(|_| std::io::Error::other("compression scheduler closed"))?;
        let data = tokio::fs::read(path).await?;
        let compressed = tokio::task::spawn_blocking(move || compress(data, encoding))
            .await
            .map_err(std::io::Error::other)??;
        Ok((Bytes::from(compressed), encoding))
    }
}

async fn resolve_path(root: &Path, uri_path: &str) -> Option<(PathBuf, bool)> {
    let decoded = percent_decode_str(uri_path).decode_utf8().ok()?;
    if decoded.contains('\0') {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in Path::new(decoded.as_ref()).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => relative.push(part),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    if relative.as_os_str().is_empty() || decoded.ends_with('/') {
        relative.push("index.html");
    }

    if let Ok(candidate) = tokio::fs::canonicalize(root.join(&relative)).await {
        if candidate.starts_with(root) && candidate.is_file() {
            return Some((candidate, false));
        }
        if candidate.starts_with(root) && candidate.is_dir() {
            if let Ok(index) = tokio::fs::canonicalize(candidate.join("index.html")).await {
                if index.starts_with(root) && index.is_file() {
                    return Some((index, false));
                }
            }
        }
    }

    let index = tokio::fs::canonicalize(root.join("index.html"))
        .await
        .ok()?;
    (index.starts_with(root) && index.is_file()).then_some((index, true))
}

async fn serve_streaming_file(
    session: &mut Session,
    path: &Path,
    metadata: &std::fs::Metadata,
    content_type: &str,
    method: &Method,
    spa_fallback: bool,
    tls: bool,
) -> Result<bool> {
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let modified_nanos = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let etag = format!("W/\"{:x}-{:x}\"", metadata.len(), modified_nanos);

    if session
        .req_header()
        .headers
        .get(IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes())
    {
        let mut response = ResponseHeader::build(304, Some(8)).unwrap();
        response.insert_header(ETAG, etag)?;
        response.insert_header(LAST_MODIFIED, httpdate::fmt_http_date(modified))?;
        insert_cache_header(&mut response, path, spa_fallback)?;
        insert_security_headers(&mut response, tls)?;
        session
            .write_response_header(Box::new(response), true)
            .await?;
        return Ok(true);
    }

    let head = *method == Method::HEAD;
    let mut file = if head {
        None
    } else {
        Some(tokio::fs::File::open(path).await.map_err(|error| {
            cloudflare_pingora::Error::because(
                cloudflare_pingora::ErrorType::FileReadError,
                "failed to open asset",
                error,
            )
        })?)
    };
    let mut response = ResponseHeader::build(200, Some(10)).unwrap();
    response.insert_header(CONTENT_TYPE, content_type)?;
    response.insert_header(CONTENT_LENGTH, metadata.len().to_string())?;
    response.insert_header(ETAG, etag)?;
    response.insert_header(LAST_MODIFIED, httpdate::fmt_http_date(modified))?;
    insert_cache_header(&mut response, path, spa_fallback)?;
    insert_security_headers(&mut response, tls)?;
    session
        .write_response_header(Box::new(response), head || metadata.len() == 0)
        .await?;

    if let Some(file) = file.as_mut() {
        let mut remaining = metadata.len();
        let mut buffer = vec![0_u8; FILE_CHUNK_BYTES];
        while remaining > 0 {
            let chunk_length = usize::try_from(remaining.min(FILE_CHUNK_BYTES as u64)).unwrap();
            let bytes = file
                .read(&mut buffer[..chunk_length])
                .await
                .map_err(|error| {
                    cloudflare_pingora::Error::because(
                        cloudflare_pingora::ErrorType::FileReadError,
                        "failed to read asset",
                        error,
                    )
                })?;
            if bytes == 0 {
                session.write_response_body(None, true).await?;
                break;
            }
            remaining -= bytes as u64;
            session
                .write_response_body(
                    Some(Bytes::copy_from_slice(&buffer[..bytes])),
                    remaining == 0,
                )
                .await?;
        }
    }
    Ok(true)
}

fn compress(data: Vec<u8>, encoding: Encoding) -> std::io::Result<Vec<u8>> {
    match encoding {
        Encoding::Identity => Ok(data),
        Encoding::Gzip => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
            encoder.write_all(&data)?;
            encoder.finish()
        }
        Encoding::Brotli => {
            let mut output = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut output, 8 * 1024, 3, 18);
                writer.write_all(&data)?;
            }
            Ok(output)
        }
        Encoding::Zstd => zstd::stream::encode_all(data.as_slice(), 1),
    }
}

fn negotiate_encoding(header: Option<&str>) -> Encoding {
    let Some(header) = header else {
        return Encoding::Identity;
    };
    let mut zstd = None;
    let mut brotli = None;
    let mut gzip = None;
    let mut wildcard = 0.0;
    for item in header.split(',') {
        let mut parts = item.trim().split(';');
        let name = parts.next().unwrap_or_default().trim();
        let mut q = 1.0;
        for parameter in parts {
            if let Some((key, value)) = parameter.trim().split_once('=') {
                if key.trim().eq_ignore_ascii_case("q") {
                    q = value.trim().parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0);
                }
            }
        }
        if name.eq_ignore_ascii_case("zstd") {
            zstd = Some(q);
        } else if name.eq_ignore_ascii_case("br") {
            brotli = Some(q);
        } else if name.eq_ignore_ascii_case("gzip") {
            gzip = Some(q);
        } else if name == "*" {
            wildcard = q;
        }
    }

    let candidates = [
        (Encoding::Zstd, zstd),
        (Encoding::Brotli, brotli),
        (Encoding::Gzip, gzip),
    ];
    let mut selected = Encoding::Identity;
    let mut selected_quality = 0.0;
    for (encoding, quality) in candidates {
        let candidate_quality = quality.unwrap_or(wildcard);
        if candidate_quality > selected_quality {
            selected = encoding;
            selected_quality = candidate_quality;
        }
    }
    selected
}

fn content_type(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_string()
}

fn compressible_path(path: &Path) -> bool {
    const EXTENSIONS: &[&str] = &[
        "css",
        "eot",
        "htm",
        "html",
        "js",
        "json",
        "ldjson",
        "map",
        "markdown",
        "md",
        "mjs",
        "otf",
        "rss",
        "svg",
        "ttf",
        "txt",
        "vtt",
        "wasm",
        "webmanifest",
        "xhtml",
        "xml",
    ];
    let known_compressible = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        });
    known_compressible || compressible_content_type(&content_type(path))
}

fn compressible_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/javascript"
                | "application/json"
                | "application/ld+json"
                | "application/manifest+json"
                | "application/wasm"
                | "application/xhtml+xml"
                | "application/xml"
                | "application/rss+xml"
                | "image/svg+xml"
                | "font/ttf"
                | "font/otf"
                | "application/vnd.ms-fontobject"
        )
}

fn insert_cache_header(
    response: &mut ResponseHeader,
    path: &Path,
    spa_fallback: bool,
) -> Result<()> {
    let index = path.file_name().is_some_and(|name| name == "index.html");
    if spa_fallback || index {
        response.insert_header(CACHE_CONTROL, "no-cache")?;
    } else {
        response.insert_header(CACHE_CONTROL, "public, max-age=2592000, immutable")?;
    }
    Ok(())
}

fn insert_security_headers(response: &mut ResponseHeader, tls: bool) -> Result<()> {
    response.insert_header("x-content-type-options", "nosniff")?;
    response.insert_header("x-frame-options", "SAMEORIGIN")?;
    response.insert_header("referrer-policy", "strict-origin-when-cross-origin")?;
    if tls {
        response.insert_header(
            "strict-transport-security",
            "max-age=63072000; includeSubDomains; preload",
        )?;
    }
    Ok(())
}

async fn send_empty(
    session: &mut Session,
    status: u16,
    headers: &[(&'static str, &str)],
    tls: bool,
) -> Result<bool> {
    let mut response = ResponseHeader::build(status, Some(headers.len() + 6)).unwrap();
    response.insert_header(CONTENT_LENGTH, "0")?;
    for (name, value) in headers {
        response.insert_header(*name, *value)?;
    }
    insert_security_headers(&mut response, tls)?;
    session
        .write_response_header(Box::new(response), true)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_negotiation_respects_quality() {
        assert_eq!(
            negotiate_encoding(Some("gzip;q=1, br;q=0.5, zstd;q=0")),
            Encoding::Gzip
        );
        assert_eq!(negotiate_encoding(Some("br, gzip")), Encoding::Brotli);
        assert_eq!(negotiate_encoding(Some("GZIP; Q=0.7")), Encoding::Gzip);
        assert_eq!(negotiate_encoding(Some("*;q=0.5, br;q=0")), Encoding::Zstd);
        assert_eq!(negotiate_encoding(None), Encoding::Identity);
    }

    #[test]
    fn compressible_extensions_are_detected_without_mime_allocation() {
        assert!(compressible_path(Path::new("asset.JSON")));
        assert!(compressible_path(Path::new("index.html")));
        assert!(!compressible_path(Path::new("audio.flac")));
    }

    #[tokio::test]
    async fn path_resolution_blocks_traversal_and_falls_back_to_spa() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.html"), "hello").unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();

        assert!(resolve_path(&root, "/../etc/passwd").await.is_none());
        let (path, fallback) = resolve_path(&root, "/app/route").await.unwrap();
        assert_eq!(path, root.join("index.html"));
        assert!(fallback);
    }

    #[test]
    fn compressors_round_trip_nonempty_data() {
        use std::io::Read;

        let data = b"pingora ".repeat(1024);
        let gzip = compress(data.clone(), Encoding::Gzip).unwrap();
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(gzip.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, data);
        assert!(!compress(data.clone(), Encoding::Brotli).unwrap().is_empty());
        assert!(!compress(data, Encoding::Zstd).unwrap().is_empty());
    }
}
