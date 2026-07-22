use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use cloudflare_pingora::http::ResponseHeader;
use cloudflare_pingora::protocols::http::conditional_filter::weak_validate_etag;
use cloudflare_pingora::proxy::Session;
use cloudflare_pingora::Result;
use http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
    IF_NONE_MATCH, LAST_MODIFIED, VARY,
};
use http::{HeaderValue, Method};
use lru::LruCache;
use parking_lot::Mutex;
use percent_encoding::percent_decode_str;
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

use crate::content_encoding::{negotiate, ContentCoding};

const MAX_BUFFERED_ASSET_BYTES: u64 = 8 * 1024 * 1024;
const FILE_CHUNK_BYTES: usize = 64 * 1024;
const ZERO: HeaderValue = HeaderValue::from_static("0");
const ACCEPT_ENCODING_VALUE: HeaderValue = HeaderValue::from_static("Accept-Encoding");
const NO_CACHE: HeaderValue = HeaderValue::from_static("no-cache");
const IMMUTABLE_CACHE: HeaderValue = HeaderValue::from_static("public, max-age=2592000, immutable");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const SAMEORIGIN: HeaderValue = HeaderValue::from_static("SAMEORIGIN");
const STRICT_REFERRER: HeaderValue = HeaderValue::from_static("strict-origin-when-cross-origin");
const HSTS: HeaderValue = HeaderValue::from_static("max-age=63072000; includeSubDomains; preload");

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
    content_type: HeaderValue,
    content_length: HeaderValue,
    etag: HeaderValue,
    last_modified: HeaderValue,
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
        if let Some((_, previous)) = state.assets.push(key, asset.clone()) {
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
    cold_read_slot: Semaphore,
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
            // Bound whole-file allocations as well as CPU-heavy compression.
            cold_read_slot: Semaphore::new(1),
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
        let negotiation = negotiate(session.req_header().headers.get_all(ACCEPT_ENCODING).iter());
        if metadata.len() > MAX_BUFFERED_ASSET_BYTES {
            if !negotiation.identity_acceptable {
                return send_empty(session, 406, &[], tls).await;
            }
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
        let requested_encoding = if metadata.len() >= 1024 && compressible_path(&path) {
            match negotiation.preferred {
                ContentCoding::Identity => Encoding::Identity,
                ContentCoding::Gzip => Encoding::Gzip,
                ContentCoding::Brotli => Encoding::Brotli,
                ContentCoding::Zstd => Encoding::Zstd,
                ContentCoding::NotAcceptable => {
                    return send_empty(session, 406, &[], tls).await;
                }
            }
        } else if negotiation.identity_acceptable {
            Encoding::Identity
        } else {
            return send_empty(session, 406, &[], tls).await;
        };
        let key = CacheKey {
            path: path.clone(),
            modified_nanos,
            length: metadata.len(),
            encoding: requested_encoding,
        };

        let mut cold_read_permit = None;
        let asset = match self.cache.get(&key) {
            Some(asset) => asset,
            None => {
                cold_read_permit = Some(self.cold_read_slot.acquire().await.map_err(|error| {
                    cloudflare_pingora::Error::because(
                        cloudflare_pingora::ErrorType::InternalError,
                        "cold read scheduler closed",
                        error,
                    )
                })?);

                // Another cold request can populate the representation while
                // this request waits for the bounded compressor.
                if let Some(asset) = self.cache.get(&key) {
                    asset
                } else {
                    let body = self
                        .read_representation(root, &path, requested_encoding)
                        .await;
                    let (body, actual_encoding) = match body {
                        Ok(value) => value,
                        Err(_) => return send_empty(session, 500, &[], tls).await,
                    };
                    let etag = format!("W/\"{:x}-{:x}\"", metadata.len(), modified_nanos);
                    let asset = Arc::new(CachedAsset {
                        content_type: cached_header_value(&content_type(&path))?,
                        content_length: cached_header_value(&body.len().to_string())?,
                        body,
                        etag: cached_header_value(&etag)?,
                        last_modified: cached_header_value(&httpdate::fmt_http_date(modified))?,
                    });
                    let actual_key = CacheKey {
                        encoding: actual_encoding,
                        ..key
                    };
                    self.cache.insert(actual_key, asset.clone());
                    asset
                }
            }
        };

        if if_none_match_matches(&session.req_header().headers, asset.etag.as_bytes()) {
            let mut response = ResponseHeader::build(304, Some(8)).unwrap();
            response.insert_header(ETAG, asset.etag.clone())?;
            response.insert_header(LAST_MODIFIED, asset.last_modified.clone())?;
            response.insert_header(VARY, ACCEPT_ENCODING_VALUE)?;
            insert_cache_header(&mut response, &path, spa_fallback)?;
            insert_security_headers(&mut response, tls)?;
            session
                .write_response_header(Box::new(response), true)
                .await?;
            return Ok(true);
        }

        let mut response = ResponseHeader::build(200, Some(12)).unwrap();
        response.insert_header(CONTENT_TYPE, asset.content_type.clone())?;
        response.insert_header(CONTENT_LENGTH, asset.content_length.clone())?;
        response.insert_header(ETAG, asset.etag.clone())?;
        response.insert_header(LAST_MODIFIED, asset.last_modified.clone())?;
        response.insert_header(VARY, ACCEPT_ENCODING_VALUE)?;
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
        drop(cold_read_permit);
        Ok(true)
    }

    async fn read_representation(
        &self,
        root: &Path,
        path: &Path,
        encoding: Encoding,
    ) -> std::io::Result<(Bytes, Encoding)> {
        if let Some(extension) = encoding.extension() {
            let mut sidecar: OsString = path.as_os_str().to_owned();
            sidecar.push(".");
            sidecar.push(extension);
            if let Some(data) = read_bounded_sidecar(root, &PathBuf::from(sidecar)).await? {
                return Ok((data, encoding));
            }
        }

        if encoding == Encoding::Identity {
            return tokio::fs::read(path)
                .await
                .map(|data| (Bytes::from(data), Encoding::Identity));
        }

        let data = tokio::fs::read(path).await?;
        let compressed = tokio::task::spawn_blocking(move || compress(data, encoding))
            .await
            .map_err(std::io::Error::other)??;
        Ok((Bytes::from(compressed), encoding))
    }
}

fn cached_header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::try_from(value).map_err(|error| {
        cloudflare_pingora::Error::because(
            cloudflare_pingora::ErrorType::InternalError,
            "generated static metadata is not a valid header value",
            error,
        )
    })
}

async fn read_bounded_sidecar(root: &Path, path: &Path) -> std::io::Result<Option<Bytes>> {
    let canonical = match tokio::fs::canonicalize(path).await {
        Ok(path) if path.starts_with(root) => path,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    let file = match tokio::fs::File::open(canonical).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.len() > MAX_BUFFERED_ASSET_BYTES {
        return Ok(None);
    }
    let mut data = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_BUFFERED_ASSET_BYTES + 1)
        .read_to_end(&mut data)
        .await?;
    if data.len() as u64 > MAX_BUFFERED_ASSET_BYTES {
        return Ok(None);
    }
    Ok(Some(Bytes::from(data)))
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
        if candidate.starts_with(root) {
            if let Ok(metadata) = tokio::fs::metadata(&candidate).await {
                if metadata.is_file() {
                    return Some((candidate, false));
                }
                if metadata.is_dir() {
                    if let Ok(index) = tokio::fs::canonicalize(candidate.join("index.html")).await {
                        if index.starts_with(root)
                            && tokio::fs::metadata(&index)
                                .await
                                .is_ok_and(|metadata| metadata.is_file())
                        {
                            return Some((index, false));
                        }
                    }
                }
            }
        }
    }

    let index = tokio::fs::canonicalize(root.join("index.html"))
        .await
        .ok()?;
    let is_file = tokio::fs::metadata(&index)
        .await
        .is_ok_and(|metadata| metadata.is_file());
    (index.starts_with(root) && is_file).then_some((index, true))
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

    if if_none_match_matches(&session.req_header().headers, etag.as_bytes()) {
        let mut response = ResponseHeader::build(304, Some(8)).unwrap();
        response.insert_header(ETAG, etag)?;
        response.insert_header(LAST_MODIFIED, httpdate::fmt_http_date(modified))?;
        response.insert_header(VARY, ACCEPT_ENCODING_VALUE)?;
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
    response.insert_header(VARY, ACCEPT_ENCODING_VALUE)?;
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

fn if_none_match_matches(headers: &http::HeaderMap, target: &[u8]) -> bool {
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .any(|value| weak_validate_etag(value.as_bytes(), target))
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
    known_compressible
        || mime_guess::from_path(path)
            .first()
            .is_some_and(|mime| compressible_content_type(mime.essence_str()))
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
        response.insert_header(CACHE_CONTROL, NO_CACHE)?;
    } else {
        response.insert_header(CACHE_CONTROL, IMMUTABLE_CACHE)?;
    }
    Ok(())
}

fn insert_security_headers(response: &mut ResponseHeader, tls: bool) -> Result<()> {
    response.insert_header("x-content-type-options", NOSNIFF)?;
    response.insert_header("x-frame-options", SAMEORIGIN)?;
    response.insert_header("referrer-policy", STRICT_REFERRER)?;
    if tls {
        response.insert_header("strict-transport-security", HSTS)?;
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
    response.insert_header(CONTENT_LENGTH, ZERO)?;
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

    #[test]
    fn if_none_match_accepts_lists_wildcards_and_weak_equivalence() {
        let mut headers = http::HeaderMap::new();
        headers.append(IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        headers.append(
            IF_NONE_MATCH,
            HeaderValue::from_static("\"miss\", \"asset\""),
        );
        assert!(if_none_match_matches(&headers, b"W/\"asset\""));
        assert!(!if_none_match_matches(&headers, b"W/\"missing\""));

        headers.clear();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match_matches(&headers, b"W/\"anything\""));

        headers.clear();
        headers.insert(
            IF_NONE_MATCH,
            HeaderValue::from_static("\"other\", \"asset,part\""),
        );
        assert!(if_none_match_matches(&headers, b"W/\"asset,part\""));
    }

    #[test]
    fn cache_byte_accounting_tracks_capacity_evictions() {
        let cache = AssetCache::new(usize::MAX);
        for index in 0..513_u128 {
            cache.insert(
                CacheKey {
                    path: PathBuf::from(format!("asset-{index}")),
                    modified_nanos: index,
                    length: 1,
                    encoding: Encoding::Identity,
                },
                Arc::new(CachedAsset {
                    body: Bytes::from_static(b"x"),
                    content_type: HeaderValue::from_static("text/plain"),
                    content_length: HeaderValue::from_static("1"),
                    etag: HeaderValue::try_from(format!("etag-{index}")).unwrap(),
                    last_modified: HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"),
                }),
            );
        }

        let state = cache.state.lock();
        let actual_bytes = state
            .assets
            .iter()
            .map(|(_, asset)| asset.body.len())
            .sum::<usize>();
        assert_eq!(state.assets.len(), 512);
        assert_eq!(state.bytes, actual_bytes);
    }

    #[tokio::test]
    async fn oversized_precompressed_sidecar_is_ignored_without_unbounded_read() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        let source = b"bounded sidecar fallback".repeat(128);
        std::fs::write(&path, &source).unwrap();
        let sidecar = directory.path().join("asset.txt.gz");
        let file = std::fs::File::create(&sidecar).unwrap();
        file.set_len(MAX_BUFFERED_ASSET_BYTES + 1).unwrap();

        let root = std::fs::canonicalize(directory.path()).unwrap();
        let files = StaticFiles::new(HashMap::new(), 1024 * 1024).unwrap();
        let (compressed, encoding) = files
            .read_representation(&root, &path, Encoding::Gzip)
            .await
            .unwrap();
        assert_eq!(encoding, Encoding::Gzip);
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_ref())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, source);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn precompressed_sidecar_symlink_cannot_escape_static_root() {
        use std::io::Read;
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        let source = b"inside static root".repeat(128);
        std::fs::write(&path, &source).unwrap();
        let outside_sidecar = outside.path().join("secret.gz");
        std::fs::write(&outside_sidecar, b"outside sentinel").unwrap();
        symlink(&outside_sidecar, directory.path().join("asset.txt.gz")).unwrap();

        let root = std::fs::canonicalize(directory.path()).unwrap();
        let files = StaticFiles::new(HashMap::new(), 1024 * 1024).unwrap();
        let (compressed, encoding) = files
            .read_representation(&root, &path, Encoding::Gzip)
            .await
            .unwrap();
        assert_eq!(encoding, Encoding::Gzip);
        assert_ne!(compressed.as_ref(), b"outside sentinel");
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_ref())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, source);
    }
}
