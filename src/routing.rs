//! Internal backend route classification and integration topology.
//!
//! Each public host handler maps request paths to a [`RouteClass`], which in turn
//! selects a prepared upstream plan (TCP/H1/H2 pool group or HTTP/3 bridge).
//! See `upstream_name_for_route()` for cross-upstream hops such as AdGuard DoH.

use crate::config::HandlerKind;
use crate::limits::LimitZone;

pub const STREAM_PREFIXES: &[&str] = &[
    "/rest/stream",
    "/rest/download",
    "/stream",
    "/play",
    "/ext/stream",
];
pub const COVER_PREFIXES: &[&str] = &["/rest/getCoverArt", "/api/artwork", "/coverart", "/artwork"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum RouteClass {
    NavidromeStream,
    NavidromeCover,
    NavidromeApi,
    VaultwardenAuth,
    VaultwardenHub,
    Vaultwarden,
    Couchdb,
    Doh,
    AdguardUi,
}

impl RouteClass {
    pub const ALL: [Self; 9] = [
        Self::NavidromeStream,
        Self::NavidromeCover,
        Self::NavidromeApi,
        Self::VaultwardenAuth,
        Self::VaultwardenHub,
        Self::Vaultwarden,
        Self::Couchdb,
        Self::Doh,
        Self::AdguardUi,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::NavidromeStream => "navidrome_stream",
            Self::NavidromeCover => "navidrome_cover",
            Self::NavidromeApi => "navidrome_api",
            Self::VaultwardenAuth => "vaultwarden_auth",
            Self::VaultwardenHub => "vaultwarden_hub",
            Self::Vaultwarden => "vaultwarden",
            Self::Couchdb => "couchdb",
            Self::Doh => "doh",
            Self::AdguardUi => "adguard_ui",
        }
    }

    pub fn default_rate_limit(self) -> Option<(f64, u32)> {
        match self {
            Self::NavidromeStream => Some((40.0, 15)),
            Self::NavidromeCover => Some((20.0, 20)),
            Self::NavidromeApi => Some((20.0, 30)),
            Self::VaultwardenAuth => Some((5.0 / 60.0, 3)),
            Self::Doh => Some((100.0, 200)),
            _ => None,
        }
    }

    pub fn timeout_seconds(self) -> u64 {
        match self {
            Self::NavidromeStream | Self::Couchdb => 3600,
            Self::VaultwardenHub => 86_400,
            Self::Vaultwarden | Self::AdguardUi => 300,
            Self::Doh => 30,
            _ => 60,
        }
    }

    pub fn upstream_pool_group(self) -> u64 {
        match self {
            Self::NavidromeStream => 1,
            Self::NavidromeCover => 2,
            Self::NavidromeApi => 3,
            Self::VaultwardenAuth => 4,
            Self::VaultwardenHub => 5,
            Self::Vaultwarden => 6,
            Self::Couchdb => 7,
            Self::Doh => 8,
            Self::AdguardUi => 9,
        }
    }

    pub fn supports_h1_bodyless_fast_path(self) -> bool {
        self != Self::VaultwardenHub
    }

    pub fn limit_zone(self) -> LimitZone {
        match self {
            Self::NavidromeStream => LimitZone::NavidromeStream,
            Self::NavidromeCover => LimitZone::NavidromeCover,
            Self::NavidromeApi => LimitZone::NavidromeApi,
            Self::VaultwardenAuth => LimitZone::VaultwardenAuth,
            Self::VaultwardenHub => LimitZone::VaultwardenHub,
            Self::Vaultwarden => LimitZone::Vaultwarden,
            Self::Couchdb => LimitZone::Couchdb,
            Self::Doh => LimitZone::Doh,
            Self::AdguardUi => LimitZone::AdguardUi,
        }
    }
}

/// Classify a request path for a configured host handler.
pub fn classify_route(handler: HandlerKind, path: &str) -> Option<RouteClass> {
    match handler {
        HandlerKind::Static => None,
        HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn => {
            if STREAM_PREFIXES
                .iter()
                .any(|prefix| path.starts_with(prefix))
            {
                Some(RouteClass::NavidromeStream)
            } else if COVER_PREFIXES.iter().any(|prefix| path.starts_with(prefix)) {
                Some(RouteClass::NavidromeCover)
            } else {
                Some(RouteClass::NavidromeApi)
            }
        }
        HandlerKind::Vaultwarden => {
            if vaultwarden_auth_path(path) {
                Some(RouteClass::VaultwardenAuth)
            } else if path.starts_with("/notifications/hub") {
                Some(RouteClass::VaultwardenHub)
            } else {
                Some(RouteClass::Vaultwarden)
            }
        }
        HandlerKind::Couchdb => Some(RouteClass::Couchdb),
        HandlerKind::AdguardDns | HandlerKind::AdguardKorea => {
            if path == "/dns-query" {
                Some(RouteClass::Doh)
            } else {
                Some(RouteClass::AdguardUi)
            }
        }
    }
}

pub fn vaultwarden_auth_path(path: &str) -> bool {
    [
        "/api/accounts/login",
        "/api/accounts/prelogin",
        "/identity/connect/token",
    ]
    .iter()
    .any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Resolve the upstream config name for a handler/route pair.
pub fn upstream_name_for_route(
    handler: HandlerKind,
    configured: Option<&str>,
    route: RouteClass,
) -> Option<&str> {
    match (handler, route) {
        (
            HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn,
            RouteClass::NavidromeStream | RouteClass::NavidromeCover | RouteClass::NavidromeApi,
        )
        | (
            HandlerKind::Vaultwarden,
            RouteClass::VaultwardenAuth | RouteClass::VaultwardenHub | RouteClass::Vaultwarden,
        )
        | (HandlerKind::Couchdb, RouteClass::Couchdb)
        | (HandlerKind::AdguardDns | HandlerKind::AdguardKorea, RouteClass::AdguardUi) => {
            configured
        }
        (HandlerKind::AdguardDns, RouteClass::Doh) => Some("adguard_dns_doh"),
        (HandlerKind::AdguardKorea, RouteClass::Doh) => Some("adguard_korea_doh"),
        _ => None,
    }
}

pub fn default_active_limit(handler: HandlerKind, route: RouteClass) -> usize {
    match handler {
        HandlerKind::NavidromeCdn if route == RouteClass::NavidromeStream => 12,
        HandlerKind::NavidromeCdn => 48,
        HandlerKind::NavidromeMain if route == RouteClass::NavidromeStream => 10,
        HandlerKind::NavidromeMain => 24,
        HandlerKind::Vaultwarden => 12,
        HandlerKind::Couchdb => 24,
        HandlerKind::AdguardDns | HandlerKind::AdguardKorea => 96,
        HandlerKind::Static => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_complete_vaultwarden_auth_prefixes() {
        assert!(vaultwarden_auth_path("/api/accounts/login"));
        assert!(vaultwarden_auth_path("/identity/connect/token/extra"));
        assert!(!vaultwarden_auth_path("/api/accounts/login-evil"));
    }

    #[test]
    fn classifies_navidrome_stream_paths() {
        assert_eq!(
            classify_route(HandlerKind::NavidromeMain, "/rest/stream/1"),
            Some(RouteClass::NavidromeStream)
        );
        assert_eq!(
            classify_route(HandlerKind::NavidromeCdn, "/play/foo"),
            Some(RouteClass::NavidromeStream)
        );
    }
}
