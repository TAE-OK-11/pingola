use std::sync::Arc;
use std::time::Duration;

use cloudflare_pingora::protocols::http::v2::server::{handshake, HttpSession};
use cloudflare_pingora::protocols::Digest;
use cloudflare_pingora::ErrorType;
use http::{Method, Request};
use tokio::io::duplex;

#[tokio::test]
async fn downstream_h2_body_read_honors_timeout() {
    let (client, server) = duplex(65536);
    let client = tokio::spawn(async move {
        let (sender, connection) = h2::client::handshake(client).await.unwrap();
        let connection = tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://www.example.com/upload")
            .body(())
            .unwrap();
        let (_response, _body) = sender
            .ready()
            .await
            .unwrap()
            .send_request(request, false)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        connection.abort();
    });

    let mut connection = handshake(Box::new(server), None).await.unwrap();
    let mut session = HttpSession::from_h2_conn(&mut connection, Arc::new(Digest::default()))
        .await
        .unwrap()
        .unwrap();
    session.set_read_timeout(Some(Duration::from_millis(10)));
    let error = session.read_body_bytes().await.unwrap_err();
    assert_eq!(error.etype(), &ErrorType::ReadTimedout);
    client.await.unwrap();
}
