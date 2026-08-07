#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}")
    p.write_text(text.replace(old, new, 1))

replace_once(
    "src/tls_policy.rs",
    'pub const HYBRID_PQ_PRIMARY_GROUP: &str = "X25519MLKEM768";\n\n',
    "",
)
replace_once(
    "src/tls_policy.rs",
    "builder.set_curves_list(HYBRID_PQ_PRIMARY_GROUP).unwrap();",
    'builder.set_curves_list("X25519MLKEM768").unwrap();',
)
replace_once(
    "src/config.rs",
    '''    if config.server.http3_max_connections_per_ip > config.server.downstream_max_connections {
        bail!("server.http3_max_connections_per_ip must not exceed server.downstream_max_connections");
    }
''',
    '''    if !config.server.http3_listen.is_empty()
        && config.server.http3_max_connections_per_ip > config.server.downstream_max_connections
    {
        bail!("server.http3_max_connections_per_ip must not exceed server.downstream_max_connections");
    }
''',
)
