#!/usr/bin/env python3
from pathlib import Path


def replace_exact(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"expected block not found in {path}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


replace_exact(
    "src/gateway.rs",
    '''fn uses_downstream_compression(route: RouteClass) -> bool {\n    matches!(\n        route,\n        RouteClass::VaultwardenAuth\n            | RouteClass::VaultwardenHub\n            | RouteClass::Vaultwarden\n            | RouteClass::Couchdb\n            | RouteClass::AdguardUi\n    )\n}\n''',
    '''fn uses_downstream_compression(route: RouteClass) -> bool {\n    matches!(\n        route,\n        RouteClass::Vaultwarden | RouteClass::Couchdb | RouteClass::AdguardUi\n    )\n}\n''',
)

replace_exact(
    "src/gateway.rs",
    '''        for route in [\n            RouteClass::VaultwardenAuth,\n            RouteClass::VaultwardenHub,\n            RouteClass::Vaultwarden,\n            RouteClass::Couchdb,\n            RouteClass::AdguardUi,\n        ] {\n            assert!(uses_downstream_compression(route), "route={route:?}");\n        }\n        for route in [\n            RouteClass::NavidromeStream,\n            RouteClass::NavidromeCover,\n            RouteClass::NavidromeApi,\n            RouteClass::Doh,\n        ] {\n            assert!(!uses_downstream_compression(route), "route={route:?}");\n        }\n''',
    '''        for route in [\n            RouteClass::Vaultwarden,\n            RouteClass::Couchdb,\n            RouteClass::AdguardUi,\n        ] {\n            assert!(uses_downstream_compression(route), "route={route:?}");\n        }\n        for route in [\n            RouteClass::NavidromeStream,\n            RouteClass::NavidromeCover,\n            RouteClass::NavidromeApi,\n            RouteClass::VaultwardenAuth,\n            RouteClass::VaultwardenHub,\n            RouteClass::Doh,\n        ] {\n            assert!(!uses_downstream_compression(route), "route={route:?}");\n        }\n''',
)

replace_exact(
    "README.md",
    '''Range/206, 이미 인코딩된 응답, `no-transform`, 1 KiB 미만, WebSocket/본문 없는 응답과\nDoH `application/dns-message` 같은 비압축 MIME은 identity를 유지합니다.\n''',
    '''Range/206, 이미 인코딩된 응답, `no-transform`, 1 KiB 미만, Vaultwarden 인증/token,\nnotifications/WebSocket Hub, 본문 없는 응답과 DoH `application/dns-message` 같은\n보안상 민감하거나 비압축 MIME은 identity를 유지합니다.\n''',
)
