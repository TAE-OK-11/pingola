#!/usr/bin/env python3
from pathlib import Path
import re

source = Path(".github/workflows/ci.yml").read_text()
replacement = """      - name: Verify unified Cloudflare BoringSSL provider
        run: |
          cargo tree --locked > /tmp/cargo-tree.txt
          cargo tree --locked -p pingora-boringssl@0.8.1 > /tmp/pingora-tls-tree.txt
          cargo tree --locked -p tokio-quiche@0.19.1 > /tmp/http3-tls-tree.txt
          cargo tree --locked -i boring@4.22.0 > /tmp/boring-reverse-tree.txt
          grep -q 'pingora-boringssl v0.8.1' /tmp/cargo-tree.txt
          grep -q 'quiche v0.29.3' /tmp/http3-tls-tree.txt
          grep -q 'boring v4.22.0' /tmp/pingora-tls-tree.txt
          grep -q 'boring-sys v4.22.0' /tmp/pingora-tls-tree.txt
          grep -q 'boring v4.22.0' /tmp/http3-tls-tree.txt
          grep -q 'boring-sys v4.22.0' /tmp/http3-tls-tree.txt
          grep -q 'pingora-boringssl v0.8.1' /tmp/boring-reverse-tree.txt
          grep -Eq '(quiche v0.29.3|tokio-quiche v0.19.1)' /tmp/boring-reverse-tree.txt
          ! grep -Eiq '(^|[[:space:]])(aws-lc-rs|aws-lc-sys|rustls) v' /tmp/cargo-tree.txt
          ! cargo tree --locked -d | grep -E '^(boring|boring-sys) v'
"""
source, count = re.subn(
    r"      - name: Verify TLS provider boundaries\n        run: \|\n.*?(?=      - name: Unit tests)",
    replacement,
    source,
    count=1,
    flags=re.S,
)
if count != 1:
    raise SystemExit(f"expected one TLS boundary block, found {count}")
source = source.replace("tls-aws-lc", "tls-boringssl")
source = source.replace("aws-lc", "boringssl")
source = source.replace("AWS-LC TCP TLS", "Cloudflare BoringSSL TLS")
Path("ci-boringssl.yml").write_text(source)
