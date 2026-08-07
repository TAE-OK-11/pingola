#!/usr/bin/env python3
from pathlib import Path


def replace_exact(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"expected block not found in {path}: {old[:80]!r}")
    p.write_text(text.replace(old, new, 1))


replace_exact(
    "src/gateway.rs",
    '''fn uses_downstream_compression(route: RouteClass) -> bool {\n    matches!(route, RouteClass::Vaultwarden | RouteClass::Couchdb)\n}\n''',
    '''fn uses_downstream_compression(route: RouteClass) -> bool {\n    matches!(\n        route,\n        RouteClass::VaultwardenAuth\n            | RouteClass::VaultwardenHub\n            | RouteClass::Vaultwarden\n            | RouteClass::Couchdb\n            | RouteClass::AdguardUi\n    )\n}\n''',
)

replace_exact(
    "src/gateway.rs",
    '''        assert!(uses_downstream_compression(RouteClass::Vaultwarden));\n        assert!(uses_downstream_compression(RouteClass::Couchdb));\n        for route in [\n            RouteClass::NavidromeStream,\n            RouteClass::NavidromeCover,\n            RouteClass::NavidromeApi,\n            RouteClass::VaultwardenAuth,\n            RouteClass::VaultwardenHub,\n            RouteClass::Doh,\n            RouteClass::AdguardUi,\n        ] {\n            assert!(!uses_downstream_compression(route), "route={route:?}");\n        }\n''',
    '''        for route in [\n            RouteClass::VaultwardenAuth,\n            RouteClass::VaultwardenHub,\n            RouteClass::Vaultwarden,\n            RouteClass::Couchdb,\n            RouteClass::AdguardUi,\n        ] {\n            assert!(uses_downstream_compression(route), "route={route:?}");\n        }\n        for route in [\n            RouteClass::NavidromeStream,\n            RouteClass::NavidromeCover,\n            RouteClass::NavidromeApi,\n            RouteClass::Doh,\n        ] {\n            assert!(!uses_downstream_compression(route), "route={route:?}");\n        }\n''',
)

replace_exact(
    "tests/backend.py",
    '''        payload = json.dumps(\n            {\n                "method": self.command,\n                "path": self.path,\n                "body_length": len(body),\n                "headers": {key.lower(): value for key, value in self.headers.items()},\n            },\n            separators=(",", ":"),\n        ).encode()\n''',
    '''        payload = json.dumps(\n            {\n                "method": self.command,\n                "path": self.path,\n                "body_length": len(body),\n                "headers": {key.lower(): value for key, value in self.headers.items()},\n                "padding": "compressible-response-" * 256\n                if self.path.startswith("/compress-large")\n                else "",\n            },\n            separators=(",", ":"),\n        ).encode()\n''',
)

replace_exact(
    "tests/fixtures/integration.yaml",
    '''  vault:\n    domains:\n      - vault.test\n    handler: vaultwarden\n    upstream: backend\n    redirect_http: true\n''',
    '''  vault:\n    domains:\n      - vault.test\n    handler: vaultwarden\n    upstream: backend\n    redirect_http: true\n  couch:\n    domains:\n      - couch.test\n    handler: couchdb\n    upstream: backend\n    redirect_http: true\n  adguard:\n    domains:\n      - dns.test\n    handler: adguard-dns\n    upstream: backend\n    redirect_http: true\n''',
)

replace_exact(
    "tests/integration.sh",
    '''  -subj "/CN=static.test" \\\n  -addext "subjectAltName=DNS:static.test,DNS:app.test,DNS:vault.test" \\\n''',
    '''  -subj "/CN=static.test" \\\n  -addext "subjectAltName=DNS:static.test,DNS:app.test,DNS:vault.test,DNS:couch.test,DNS:dns.test" \\\n''',
)

needle = '''jq -e '.headers["x-forwarded-port"] == "443"' \\\n  <<<"${proxy_response}" >/dev/null\n\n'''
insert = needle + r'''# Modern downstream compression prefers zstd, then Brotli, then gzip for
# eligible non-Navidrome proxy responses. The backend always emits identity so
# these assertions prove the gateway itself performed the transformation.
curl --noproxy '*' -ksS --http2 --raw -D "${RUNTIME}/vault-zstd.headers" \
  --resolve vault.test:443:127.0.0.1 \
  -H 'accept-encoding: gzip, br, zstd' \
  https://vault.test:443/compress-large -o "${RUNTIME}/vault-zstd.body"
grep -qi '^content-encoding: zstd' "${RUNTIME}/vault-zstd.headers"
grep -qi '^vary:.*accept-encoding' "${RUNTIME}/vault-zstd.headers"
[[ -s "${RUNTIME}/vault-zstd.body" ]]

curl --noproxy '*' -ksS --http2 --raw -D "${RUNTIME}/couch-br.headers" \
  --resolve couch.test:443:127.0.0.1 \
  -H 'accept-encoding: gzip, br' \
  https://couch.test:443/compress-large -o "${RUNTIME}/couch-br.body"
grep -qi '^content-encoding: br' "${RUNTIME}/couch-br.headers"
[[ -s "${RUNTIME}/couch-br.body" ]]

curl --noproxy '*' -ksS --http2 --raw -D "${RUNTIME}/adguard-gzip.headers" \
  --resolve dns.test:443:127.0.0.1 \
  -H 'accept-encoding: gzip' \
  https://dns.test:443/compress-large -o "${RUNTIME}/adguard-gzip.body"
grep -qi '^content-encoding: gzip' "${RUNTIME}/adguard-gzip.headers"
[[ -s "${RUNTIME}/adguard-gzip.body" ]]

# Navidrome remains outside the gateway compression module. Its API/cover path
# may still forward Accept-Encoding to the origin, but the proxy itself must not
# add a Content-Encoding when the origin returned identity.
curl --noproxy '*' -ksS --http2 --raw -D "${RUNTIME}/navidrome.headers" \
  --resolve app.test:443:127.0.0.1 \
  -H 'accept-encoding: gzip, br, zstd' \
  https://app.test:443/compress-large -o "${RUNTIME}/navidrome.body"
if grep -qi '^content-encoding:' "${RUNTIME}/navidrome.headers"; then
  echo 'Navidrome response was unexpectedly compressed by the gateway' >&2
  exit 1
fi

'''
replace_exact("tests/integration.sh", needle, insert)

replace_exact(
    "README.md",
    '''Navidrome API/cover는 client의 `Accept-Encoding`을 origin에\n전달하고, Vaultwarden 일반 API와 CouchDB의 압축 가능한 응답은 Pingora가 level 1\ngzip으로 streaming 압축합니다. Audio stream, Vaultwarden 인증·notifications hub 및\nDoH는 압축하지 않습니다.\n''',
    '''Navidrome API/cover는 client의 `Accept-Encoding`을 origin에 전달하고 gateway\n압축은 적용하지 않습니다. 그 외 압축 가능한 프록시 응답(Vaultwarden, CouchDB,\nAdGuard UI)은 client의 `Accept-Encoding`을 정확히 협상해 같은 q-value에서는\n`zstd` → `br` → `gzip` 순으로 선택하고 level 1 streaming 압축을 적용합니다.\nRange/206, 이미 인코딩된 응답, `no-transform`, 1 KiB 미만, WebSocket/본문 없는 응답과\nDoH `application/dns-message` 같은 비압축 MIME은 identity를 유지합니다.\n''',
)
