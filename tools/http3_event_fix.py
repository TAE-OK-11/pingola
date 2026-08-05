#!/usr/bin/env python3
from pathlib import Path

path = Path("src/http3.rs")
text = path.read_text()
old = '''        match event {
            ServerH3Event::Core(H3Event::IncomingHeaders(incoming)) => {
                tokio::spawn(proxy_request(
                    incoming,
                    peer,
                    internal,
                    public_port,
                    client.clone(),
                    alt_svc.clone(),
                ));
            }
            ServerH3Event::Core(H3Event::BodyBytesReceived { .. }) => {}
            ServerH3Event::Core(event) => {
                log::debug!("HTTP/3 connection event peer={peer}: {event:?}");
            }
            event => log::debug!("HTTP/3 server event peer={peer}: {event:?}"),
        }
'''
new = '''        match event {
            ServerH3Event::Headers {
                incoming_headers,
                is_in_early_data,
                ..
            } => {
                if *is_in_early_data {
                    warn!("HTTP/3 early-data request rejected peer={peer}");
                    let IncomingH3Headers { mut send, .. } = incoming_headers;
                    if let Err(error) = send_error(
                        &mut send,
                        StatusCode::TOO_EARLY,
                        "HTTP/3 early data is not accepted",
                    )
                    .await
                    {
                        warn!("failed to reject HTTP/3 early-data request peer={peer}: {error:#}");
                    }
                    continue;
                }
                tokio::spawn(proxy_request(
                    incoming_headers,
                    peer,
                    internal,
                    public_port,
                    client.clone(),
                    alt_svc.clone(),
                ));
            }
            ServerH3Event::Core(H3Event::BodyBytesReceived { .. }) => {}
            ServerH3Event::Core(event) => {
                log::debug!("HTTP/3 connection event peer={peer}: {event:?}");
            }
        }
'''
if text.count(old) != 1:
    raise SystemExit("expected exactly one outdated HTTP/3 server event match")
path.write_text(text.replace(old, new))
