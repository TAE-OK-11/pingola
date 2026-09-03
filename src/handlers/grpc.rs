//! Minimal upstream gRPC / gRPC-web policy.
//!
//! Vaultwarden Hub stays a path-based H1 WebSocket exception. gRPC is detected
//! only from `Content-Type`, so Hub classification, pool groups, and ALPN are
//! unchanged.

use cloudflare_pingora::http::RequestHeader;
use cloudflare_pingora::protocols::http::bridge::grpc_web::GrpcWebCtx;
use http::HeaderValue;
use http::header::{CONTENT_TYPE, TE};

const TE_TRAILERS: HeaderValue = HeaderValue::from_static("trailers");
const GRPC_PREFIX: &[u8] = b"application/grpc";
const GRPC_WEB_SUFFIX: &[u8] = b"-web";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrpcKind {
    Native,
    Web,
}

pub fn classify_request(request: &RequestHeader) -> Option<GrpcKind> {
    request
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| classify_content_type(value.as_bytes()))
}

pub fn classify_content_type(value: &[u8]) -> Option<GrpcKind> {
    let value = value.trim_ascii_start();
    if value.len() < GRPC_PREFIX.len()
        || !value[..GRPC_PREFIX.len()].eq_ignore_ascii_case(GRPC_PREFIX)
    {
        return None;
    }
    let rest = &value[GRPC_PREFIX.len()..];
    if rest.is_empty() || rest[0] == b'+' || rest[0] == b';' || rest[0].is_ascii_whitespace() {
        return Some(GrpcKind::Native);
    }
    if rest.len() >= GRPC_WEB_SUFFIX.len()
        && rest[..GRPC_WEB_SUFFIX.len()].eq_ignore_ascii_case(GRPC_WEB_SUFFIX)
    {
        return Some(GrpcKind::Web);
    }
    None
}

/// Convert gRPC-web to native gRPC, or mark a native gRPC request for H2 EOS.
///
/// Returns whether the request used the gRPC-web bridge. Hub and every other
/// content type take the `None` path with no extra headers or modules.
pub fn prepare_upstream_request(request: &mut RequestHeader, grpc_web: &mut GrpcWebCtx) -> bool {
    match classify_request(request) {
        Some(GrpcKind::Web) => {
            grpc_web.init();
            grpc_web.request_header_filter(request);
            true
        }
        Some(GrpcKind::Native) => {
            request.insert_typed_header(TE, TE_TRAILERS);
            request.set_send_end_stream(false);
            false
        }
        None => false,
    }
}

pub fn apply_web_response(
    grpc_web: &mut GrpcWebCtx,
    response: &mut cloudflare_pingora::http::ResponseHeader,
) {
    if *grpc_web == GrpcWebCtx::Disabled {
        return;
    }
    grpc_web.response_header_filter(response);
}

#[cfg(test)]
mod tests {
    use cloudflare_pingora::http::RequestHeader;
    use http::{Method, Version};

    use super::*;

    fn request_with_type(content_type: &str) -> RequestHeader {
        let mut request = RequestHeader::build(Method::POST, b"/pkg.Svc/Method", None).unwrap();
        request.set_version(Version::HTTP_2);
        request.insert_header(CONTENT_TYPE, content_type).unwrap();
        request
    }

    #[test]
    fn classifies_native_and_web_content_types() {
        assert_eq!(
            classify_content_type(b"application/grpc"),
            Some(GrpcKind::Native)
        );
        assert_eq!(
            classify_content_type(b"Application/gRPC+proto"),
            Some(GrpcKind::Native)
        );
        assert_eq!(
            classify_content_type(b"application/grpc; charset=utf-8"),
            Some(GrpcKind::Native)
        );
        assert_eq!(
            classify_content_type(b"application/grpc-web"),
            Some(GrpcKind::Web)
        );
        assert_eq!(
            classify_content_type(b"application/grpc-web+proto"),
            Some(GrpcKind::Web)
        );
        assert_eq!(classify_content_type(b"application/json"), None);
        assert_eq!(classify_content_type(b"application/grpcish"), None);
    }

    #[test]
    fn native_grpc_keeps_te_trailers_and_disables_header_eos() {
        let mut request = request_with_type("application/grpc+proto");
        let mut grpc_web = GrpcWebCtx::default();
        assert!(!prepare_upstream_request(&mut request, &mut grpc_web));
        assert_eq!(request.headers.get(TE).unwrap(), "trailers");
        assert_eq!(request.send_end_stream(), Some(false));
        assert_eq!(grpc_web, GrpcWebCtx::Disabled);
    }

    #[test]
    fn grpc_web_converts_to_native_grpc() {
        let mut request = request_with_type("application/grpc-web+proto");
        let mut grpc_web = GrpcWebCtx::default();
        assert!(prepare_upstream_request(&mut request, &mut grpc_web));
        assert_eq!(
            request.headers.get(CONTENT_TYPE).unwrap(),
            "application/grpc+proto"
        );
        assert_eq!(request.headers.get(TE).unwrap(), "trailers");
        assert_eq!(request.send_end_stream(), Some(false));
        assert_eq!(grpc_web, GrpcWebCtx::Upgrade);
    }

    #[test]
    fn non_grpc_requests_are_untouched() {
        let mut request = request_with_type("application/json");
        let mut grpc_web = GrpcWebCtx::default();
        assert!(!prepare_upstream_request(&mut request, &mut grpc_web));
        assert!(request.headers.get(TE).is_none());
        assert_eq!(request.send_end_stream(), Some(true));
        assert_eq!(grpc_web, GrpcWebCtx::Disabled);
    }
}
