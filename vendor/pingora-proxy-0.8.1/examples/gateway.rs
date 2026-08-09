// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use async_trait::async_trait;
use bytes::Bytes;
use log::info;
use prometheus::register_int_counter;

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};

fn check_login(req: &pingora_http::RequestHeader, expected: &http::HeaderValue) -> bool {
    req.headers.get(http::header::AUTHORIZATION) == Some(expected)
}

pub struct MyGateway {
    req_metric: prometheus::IntCounter,
    expected_authorization: http::HeaderValue,
}

#[async_trait]
impl ProxyHttp for MyGateway {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        if session.req_header().uri.path().starts_with("/login")
            && !check_login(session.req_header(), &self.expected_authorization)
        {
            let _ = session
                .respond_error_with_body(403, Bytes::from_static(b"no way!"))
                .await;
            // true: early return as the response is already written
            return Ok(true);
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let addr = if session.req_header().uri.path().starts_with("/family") {
            ("1.0.0.1", 443)
        } else {
            ("1.1.1.1", 443)
        };

        info!("connecting to {addr:?}");

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        // replace existing header if any
        upstream_response
            .insert_header("Server", "MyGateway")
            .unwrap();
        // because we don't support h3
        upstream_response.remove_header("alt-svc");

        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) {
        let response_code = session
            .response_written()
            .map_or(0, |resp| resp.status.as_u16());
        info!(
            "{} response code: {response_code}",
            self.request_summary(session, ctx)
        );

        self.req_metric.inc();
    }
}

// PINGORA_EXAMPLE_TOKEN=replace-me RUST_LOG=INFO cargo run --example gateway
// curl 127.0.0.1:6191 -H "Host: one.one.one.one"
// curl 127.0.0.1:6190/family/ -H "Host: one.one.one.one"
// curl 127.0.0.1:6191/login/ -H "Host: one.one.one.one" -I -H "Authorization: Bearer replace-me"
// curl 127.0.0.1:6191/login/ -H "Host: one.one.one.one" -I -H "Authorization: bad"
// For metrics
// curl 127.0.0.1:6192/
fn main() {
    env_logger::init();
    let token = std::env::var("PINGORA_EXAMPLE_TOKEN")
        .expect("set PINGORA_EXAMPLE_TOKEN before running this authentication example");
    let expected_authorization = http::HeaderValue::try_from(format!("Bearer {token}"))
        .expect("PINGORA_EXAMPLE_TOKEN must form a valid Authorization header");

    // read command line arguments
    let opt = Opt::parse_args();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        MyGateway {
            req_metric: register_int_counter!("req_counter", "Number of requests").unwrap(),
            expected_authorization,
        },
    );
    my_proxy.add_tcp("127.0.0.1:6191");
    my_server.add_service(my_proxy);

    let mut prometheus_service_http =
        pingora_core::services::listening::Service::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6192");
    my_server.add_service(prometheus_service_http);

    my_server.run_forever();
}
