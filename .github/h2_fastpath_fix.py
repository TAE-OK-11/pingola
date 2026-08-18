from pathlib import Path
p = Path('vendor/pingora-core-0.8.1/src/connectors/http/v2.rs')
text = p.read_text()
old = '''                let cached_peer_matches = conn
                    .digest()
                    .socket_digest
                    .as_ref()
                    .is_some_and(|digest| peer.matches_cached_peer_addr(digest.peer_addr()));'''
new = '''                let cached_peer_matches = conn
                    .digest()
                    .socket_digest
                    .as_ref()
                    .and_then(|digest| digest.peer_addr())
                    .is_some_and(|addr| peer.matches_cached_peer_addr(addr));'''
if text.count(old) != 1:
    raise SystemExit(f'expected one fast-path match, found {text.count(old)}')
p.write_text(text.replace(old, new, 1))
