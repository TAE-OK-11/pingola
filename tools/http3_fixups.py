#!/usr/bin/env python3
from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    file = Path(path)
    text = file.read_text()
    actual = text.count(old)
    if actual != count:
        raise SystemExit(f"{path}: expected {count} matches, found {actual}: {old!r}")
    file.write_text(text.replace(old, new))


replace(
    "src/http3.rs",
    "use futures::{StreamExt, stream};",
    "use futures::{SinkExt, StreamExt, stream};",
)
replace(
    "src/http3.rs",
    "use hyper_util::client::legacy::{Client, ResponseFuture};",
    "use hyper_util::client::legacy::Client;",
)
replace(
    "src/http3.rs",
    "use tokio_quiche::quic::SimpleConnectionIdGenerator;\n",
    "",
)
replace(
    "src/http3.rs",
    "    let mut quic = QuicSettings::default();\n",
    "    let mut quic = QuicSettings::default();\n    quic.enable_dgram = false;\n",
)
replace(
    "src/http3.rs",
    '''    let listeners = listen(
        sockets,
        params,
        SimpleConnectionIdGenerator,
        DefaultMetrics,
    )
    .context("failed to create quiche HTTP/3 listeners")?;''',
    '''    let listeners = listen(sockets, params, DefaultMetrics)
        .context("failed to create quiche HTTP/3 listeners")?;''',
)
replace(
    "src/http3.rs",
    "        let internal = internal;\n",
    "",
)
replace(
    "src/gateway.rs",
    ".is_some_and(|address| address == expected);",
    ".is_some_and(|address| *address == expected);",
)
