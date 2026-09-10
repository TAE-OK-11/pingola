use std::sync::Arc;
use std::time::Duration;

use cloudflare_pingora::ErrorType;
use cloudflare_pingora::protocols::Digest;
use cloudflare_pingora::protocols::http::v2::server::{H2Accept, HttpSession, handshake};
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
    let H2Accept::Session(mut session) =
        HttpSession::from_h2_conn(&mut connection, Arc::new(Digest::default()))
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("valid test request was rejected during H2 acceptance");
    };
    session.set_read_timeout(Some(Duration::from_millis(10)));
    let error = session.read_body_bytes().await.unwrap_err();
    assert_eq!(error.etype(), &ErrorType::ReadTimedout);
    client.await.unwrap();
}

#[tokio::test]
async fn downstream_cancel_has_distinct_context_from_protocol_errors() {
    for reason in [h2::Reason::CANCEL, h2::Reason::PROTOCOL_ERROR] {
        let (client, server) = duplex(65536);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let client = tokio::spawn(async move {
            let (sender, connection) = h2::client::handshake(client).await.unwrap();
            let connection = tokio::spawn(async move {
                let _ = connection.await;
            });
            let request = Request::builder()
                .uri("https://www.example.com/rest/getCoverArt.view")
                .body(())
                .unwrap();
            let (_response, mut body) = sender
                .ready()
                .await
                .unwrap()
                .send_request(request, true)
                .unwrap();
            ready_rx.await.unwrap();
            body.send_reset(reason);
            let _ = done_rx.await;
            connection.abort();
        });
        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let H2Accept::Session(mut session) =
            HttpSession::from_h2_conn(&mut connection, Arc::new(Digest::default()))
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("valid test request was rejected during H2 acceptance");
        };
        let driver = tokio::spawn(async move { while connection.accept().await.is_some() {} });
        ready_tx.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), session.read_body_or_idle(true))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.etype(), &ErrorType::H2Error);
        let context = error.context.as_ref().unwrap().as_str();
        assert_eq!(
            context == "Client closed H2, reason: stream no longer needed",
            reason == h2::Reason::CANCEL
        );
        let _ = done_tx.send(());
        client.await.unwrap();
        driver.abort();
    }
}
