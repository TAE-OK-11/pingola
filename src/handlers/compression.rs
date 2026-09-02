use cloudflare_pingora::modules::http::HttpModules;
use cloudflare_pingora::modules::http::compression::ResponseCompression;
use cloudflare_pingora::proxy::Session;
use cloudflare_pingora::{Error, ErrorType, Result};
use http::header::ACCEPT_ENCODING;

use crate::content_encoding::{ContentCoding, EncodingNegotiation, negotiate};
use crate::routing::RouteClass;

/// Let selected application origins negotiate their own response encoding.
pub fn forwards_accept_encoding(route: RouteClass) -> bool {
    matches!(route, RouteClass::NavidromeApi | RouteClass::NavidromeCover)
}

pub fn uses_downstream_compression(route: RouteClass) -> bool {
    matches!(
        route,
        RouteClass::Vaultwarden | RouteClass::Couchdb | RouteClass::AdguardUi
    )
}

pub fn configure_downstream_compression(
    session: &mut Session,
    route: RouteClass,
    compression_modules: &HttpModules,
) -> Result<EncodingNegotiation> {
    let encoding = negotiate_downstream_compression(session, route)?;
    if encoding.preferred == ContentCoding::NotAcceptable {
        return Ok(encoding);
    }
    if let Some(preferred) = encoding.preferred.as_str() {
        install_downstream_compression(session, preferred, compression_modules)?;
    }
    Ok(encoding)
}

pub fn negotiate_downstream_compression(
    session: &Session,
    route: RouteClass,
) -> Result<EncodingNegotiation> {
    if !uses_downstream_compression(route) {
        return Ok(EncodingNegotiation {
            preferred: ContentCoding::Identity,
            identity_acceptable: true,
        });
    }

    let mut values = session.req_header().headers.get_all(ACCEPT_ENCODING).iter();
    if let Some(first) = values.next()
        && values.next().is_none()
        && first.as_bytes().eq_ignore_ascii_case(b"identity")
    {
        return Ok(EncodingNegotiation {
            preferred: ContentCoding::Identity,
            identity_acceptable: true,
        });
    }

    Ok(negotiate(
        session.req_header().headers.get_all(ACCEPT_ENCODING).iter(),
    ))
}

pub fn install_downstream_compression(
    session: &mut Session,
    preferred_encoding: &'static str,
    compression_modules: &HttpModules,
) -> Result<()> {
    session
        .downstream_session
        .req_header_mut()
        .insert_typed_header(
            ACCEPT_ENCODING,
            http::HeaderValue::from_static(preferred_encoding),
        );
    session.downstream_modules_ctx = compression_modules.build_ctx();
    let request = session.downstream_session.req_header();
    let Some(compression) = session
        .downstream_modules_ctx
        .get_mut::<ResponseCompression>()
    else {
        return Error::e_explain(
            ErrorType::InternalError,
            "failed to initialize selected response compressor",
        );
    };
    compression.request_filter(request);
    Ok(())
}
