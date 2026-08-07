from pathlib import Path
import re

path = Path("src/upstream_h3.rs")
text = path.read_text()

text = text.replace("use std::convert::Infallible;\n", "")
text = text.replace(
    "use http_body_util::{BodyExt, Empty, StreamBody};",
    "use http_body_util::{BodyExt, StreamBody};",
)

text, count = re.subn(
    r"\n\s*pub fn is_empty\(&self\) -> bool \{\s*self\.routes\.is_empty\(\)\s*\}\n",
    "\n",
    text,
    count=1,
)
if count != 1:
    raise SystemExit("unused UpstreamH3Registry::is_empty not found")

start = text.find("    async fn send_body(&self, data: Bytes, fin: bool) -> Result<(), BoxError> {")
end = text.find("    async fn response(self) -> Result<ResponseHead, BoxError> {", start)
if start == -1 or end == -1:
    raise SystemExit("unused RequestHandle body/trailer methods not found")
text = text[:start] + text[end:]

# Remove only PendingRequest's redundant id; Command variants and RequestHandle still use IDs.
text, count = re.subn(
    r"(struct PendingRequest \{\n)\s*id: u64,\n",
    r"\1",
    text,
    count=1,
)
if count != 1:
    raise SystemExit("PendingRequest.id field not found")
text, count = re.subn(
    r"(PendingRequest \{\n)\s*id,\n",
    r"\1",
    text,
    count=1,
)
if count != 1:
    raise SystemExit("PendingRequest.id initializer not found")

finished_pattern = re.compile(
    r'''            h3::Event::Finished => \{\n'''
    r'''                stream_to_request\.remove\(&stream_id\);\n'''
    r'''                if let Some\(mut request\) = requests\.remove\(&request_id\) \{\n'''
    r'''                    if !request\.response_started \{\n'''
    r'''                        if let Some\(response\) = request\.response\.take\(\) \{\n'''
    r'''                            let _ = response(?:\n\s*)?\.send\(Err\("HTTP/3 response finished before headers"\.into\(\)\)\);\n'''
    r'''                        \}\n'''
    r'''                    \}\n'''
    r'''                \}\n'''
    r'''            \}'''
)
new_finished = '''            h3::Event::Finished => {
                stream_to_request.remove(&stream_id);
                if let Some(mut request) = requests.remove(&request_id)
                    && !request.response_started
                    && let Some(response) = request.response.take()
                {
                    let _ = response.send(Err("HTTP/3 response finished before headers".into()));
                }
            }'''
text, count = finished_pattern.subn(new_finished, text, count=1)
if count != 1:
    raise SystemExit("Finished event clippy block not found")

body_pattern = re.compile(
    r'''                if let Some\(data\) = frame\.data_ref\(\) \{\n'''
    r'''                    if commands\n'''
    r'''                        \.send\(Command::Body \{\n'''
    r'''                            id,\n'''
    r'''                            data: data\.clone\(\),\n'''
    r'''                            fin: false,\n'''
    r'''                        \}\)\n'''
    r'''                        \.await\n'''
    r'''                        \.is_err\(\)\n'''
    r'''                    \{\n'''
    r'''                        return;\n'''
    r'''                    \}\n'''
    r'''                \}'''
)
new_body = '''                if let Some(data) = frame.data_ref()
                    && commands
                        .send(Command::Body {
                            id,
                            data: data.clone(),
                            fin: false,
                        })
                        .await
                        .is_err()
                {
                    return;
                }'''
text, count = body_pattern.subn(new_body, text, count=1)
if count != 1:
    raise SystemExit("request body clippy block not found")

text = text.replace("item.map_err(|message| boxed_error(message))", "item.map_err(boxed_error)")

text, count = re.subn(
    r'''\nfn empty_body\(\) -> BridgeBody \{.*?\n\}\n(?=\n#\[cfg\(test\)\])''',
    "\n",
    text,
    count=1,
    flags=re.DOTALL,
)
if count != 1:
    raise SystemExit("unused empty_body helper not found")

path.write_text(text)
