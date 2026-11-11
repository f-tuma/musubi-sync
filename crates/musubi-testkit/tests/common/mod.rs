//! Minimal HTTP/1.1 client and webhook receiver for the fake Google server tests.

#![allow(dead_code, reason = "each test binary uses a subset")]

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::sync::mpsc;

/// A response: status, headers and the body parsed as JSON (`Null` if empty).
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub json: Value,
}

async fn send(url: &str, req: Request<Body>) -> Resp {
    let uri: axum::http::Uri = url.parse().expect("url");
    let stream = tokio::net::TcpStream::connect(uri.authority().expect("host").as_str())
        .await
        .expect("connect");
    let (mut tx, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::spawn(conn);
    let resp = tx.send_request(req).await.expect("request");
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(Body::new(body), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON body")
    };
    Resp {
        status: parts.status,
        headers: parts.headers,
        json,
    }
}

fn builder(method: &str, url: &str, token: Option<&str>) -> axum::http::request::Builder {
    let uri: axum::http::Uri = url.parse().expect("url");
    let mut b = Request::builder()
        .method(method)
        .uri(uri.path_and_query().map_or("/", |p| p.as_str()))
        .header("host", uri.authority().expect("host").as_str());
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b
}

pub async fn get(url: &str, token: &str) -> Resp {
    send(
        url,
        builder("GET", url, Some(token))
            .body(Body::empty())
            .expect("req"),
    )
    .await
}

pub async fn get_anon(url: &str) -> Resp {
    send(
        url,
        builder("GET", url, None).body(Body::empty()).expect("req"),
    )
    .await
}

pub async fn post_json(url: &str, token: &str, body: &Value) -> Resp {
    let req = builder("POST", url, Some(token))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("req");
    send(url, req).await
}

pub async fn post_form(url: &str, form: &str) -> Resp {
    let req = builder("POST", url, None)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .expect("req");
    send(url, req).await
}

pub fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().expect("object").keys().cloned().collect()
}

pub fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

/// A webhook endpoint on 127.0.0.1; every POST's headers arrive on the channel.
pub async fn receiver() -> (String, mpsc::UnboundedReceiver<HeaderMap>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |h: HeaderMap| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(h);
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}/hook"), rx)
}
