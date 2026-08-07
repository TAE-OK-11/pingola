from pathlib import Path
import re


def replace(path, old, new, count=1):
    p = Path(path)
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"missing replacement in {path}: {old[:140]!r}")
    p.write_text(text.replace(old, new, count))

# Downstream early-data: avoid borrowing RuntimeConfig into the spawned 'static task,
# and pass IncomingH3Headers by reference to the replay-safety predicate.
replace(
    "src/http3.rs",
    "    let server = &runtime.config.server;\n",
    "    let server = &runtime.config.server;\n    let allow_early_data = server.http3_enable_early_data;\n",
)
text = Path("src/http3.rs").read_text()
text = text.replace("quic.enable_early_data = server.http3_enable_early_data;", "quic.enable_early_data = allow_early_data;")
text = text.replace("allow_early_data: server.http3_enable_early_data,", "allow_early_data,")
text = text.replace("server.http3_enable_early_data,\n        HTTP3_MAX_UDP_PAYLOAD_SIZE,", "allow_early_data,\n        HTTP3_MAX_UDP_PAYLOAD_SIZE,")
text = text.replace("early_data_request_is_replay_safe(incoming_headers)", "early_data_request_is_replay_safe(&incoming_headers)")
Path("src/http3.rs").write_text(text)

# Existing gateway unit tests call prepare_upstream directly; provide an empty
# H3 registry for the legacy H1/H2-only test cases.
p = Path("src/gateway.rs")
text = p.read_text()
pattern = re.compile(r'prepare_upstream\(("[^"]+"),\s*&([A-Za-z_][A-Za-z0-9_]*)\)')
text, substitutions = pattern.subn(
    r'prepare_upstream(\1, &\2, &UpstreamH3Registry::default())',
    text,
)
if substitutions == 0:
    raise SystemExit("no direct prepare_upstream test calls found")
p.write_text(text)

# quiche 0.29.3 and Rust ownership fixes in the low-level H3 worker.
p = Path("src/upstream_h3.rs")
text = p.read_text()
text = text.replace("use futures::{StreamExt, stream};", "use futures::stream;")
text = text.replace("#[derive(Clone)]\npub struct BridgeRoute", "#[derive(Clone, Debug)]\npub struct BridgeRoute")
text = text.replace(
    "    opened: oneshot::Receiver<Result<(), String>>,\n",
    "    opened: Option<oneshot::Receiver<Result<(), String>>>,\n",
)
text = text.replace(
    "            opened: opened_rx,\n",
    "            opened: Some(opened_rx),\n",
)
old = '''    async fn wait_opened(&mut self) -> Result<(), BoxError> {\n        self.opened\n            .await\n            .map_err(|_| boxed_error("upstream HTTP/3 request open channel closed"))?\n            .map_err(boxed_error)\n    }'''
new = '''    async fn wait_opened(&mut self) -> Result<(), BoxError> {\n        let opened = self\n            .opened\n            .take()\n            .ok_or_else(|| boxed_error("upstream HTTP/3 request open channel was already consumed"))?;\n        opened\n            .await\n            .map_err(|_| boxed_error("upstream HTTP/3 request open channel closed"))?\n            .map_err(boxed_error)\n    }'''
if old not in text:
    raise SystemExit("RequestHandle::wait_opened pattern missing")
text = text.replace(old, new, 1)
text = text.replace(
    ".send_additional_headers(conn, stream_id, &headers, true)",
    ".send_additional_headers(conn, stream_id, &headers, true, true)",
)
p.write_text(text)
