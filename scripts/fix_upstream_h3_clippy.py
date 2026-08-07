from pathlib import Path

path = Path("src/upstream_h3.rs")
text = path.read_text()

text = text.replace("use std::convert::Infallible;\n", "")
text = text.replace("use http_body_util::{BodyExt, Empty, StreamBody};", "use http_body_util::{BodyExt, StreamBody};")

text = text.replace(
    '''\n    pub fn is_empty(&self) -> bool {\n        self.routes.is_empty()\n    }\n''',
    "\n",
)

start = text.find("    async fn send_body(&self, data: Bytes, fin: bool) -> Result<(), BoxError> {")
end = text.find("    async fn response(self) -> Result<ResponseHead, BoxError> {", start)
if start == -1 or end == -1:
    raise SystemExit("unused RequestHandle body/trailer methods not found")
text = text[:start] + text[end:]

text = text.replace("    id: u64,\n    headers: Vec<h3::Header>,", "    headers: Vec<h3::Header>,")
text = text.replace("                    id,\n                    headers,", "                    headers,")

old_finished = '''            h3::Event::Finished => {\n                stream_to_request.remove(&stream_id);\n                if let Some(mut request) = requests.remove(&request_id) {\n                    if !request.response_started {\n                        if let Some(response) = request.response.take() {\n                            let _ = response\n                                .send(Err("HTTP/3 response finished before headers".into()));\n                        }\n                    }\n                }\n            }'''
new_finished = '''            h3::Event::Finished => {\n                stream_to_request.remove(&stream_id);\n                if let Some(mut request) = requests.remove(&request_id)\n                    && !request.response_started\n                    && let Some(response) = request.response.take()\n                {\n                    let _ = response.send(Err("HTTP/3 response finished before headers".into()));\n                }\n            }'''
if old_finished not in text:
    raise SystemExit("Finished event clippy block not found")
text = text.replace(old_finished, new_finished, 1)

old_data = '''                if let Some(data) = frame.data_ref() {\n                    if commands\n                        .send(Command::Body {\n                            id,\n                            data: data.clone(),\n                            fin: false,\n                        })\n                        .await\n                        .is_err()\n                    {\n                        return;\n                    }\n                }'''
new_data = '''                if let Some(data) = frame.data_ref()\n                    && commands\n                        .send(Command::Body {\n                            id,\n                            data: data.clone(),\n                            fin: false,\n                        })\n                        .await\n                        .is_err()\n                {\n                    return;\n                }'''
if old_data not in text:
    raise SystemExit("request body clippy block not found")
text = text.replace(old_data, new_data, 1)

text = text.replace("item.map_err(|message| boxed_error(message))", "item.map_err(boxed_error)")

start = text.find("\nfn empty_body() -> BridgeBody {")
if start == -1:
    raise SystemExit("unused empty_body helper not found")
end = text.find("\n#[cfg(test)]", start)
if end == -1:
    raise SystemExit("test module after empty_body not found")
text = text[:start] + "\n" + text[end:]

path.write_text(text)
