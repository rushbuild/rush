// Simple HTTP cache server for Rush
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Simple HTTP cache server for testing remote cache functionality.
//!
//! Usage: cargo run --example http_cache_server

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use warp::Filter;

type Cache = Arc<Mutex<HashMap<String, Vec<u8>>>>;

#[tokio::main]
async fn main() {
    let cache: Cache = Arc::new(Mutex::new(HashMap::new()));

    // Health check endpoint
    let health =
        warp::path("health").map(|| warp::reply::with_status("OK", warp::http::StatusCode::OK));

    // GET /ac/:key - retrieve action result
    let cache_get = cache.clone();
    let get_route = warp::path!("ac" / String)
        .and(warp::get())
        .map(move |key: String| {
            let cache_lock = cache_get.lock().unwrap();
            match cache_lock.get(&key) {
                Some(data) => {
                    println!("GET /ac/{} - HIT", key);
                    warp::reply::with_status(data.clone(), warp::http::StatusCode::OK)
                }
                None => {
                    println!("GET /ac/{} - MISS", key);
                    warp::reply::with_status(Vec::new(), warp::http::StatusCode::NOT_FOUND)
                }
            }
        });

    // PUT /ac/:key - store action result
    let cache_put = cache.clone();
    let put_route = warp::path!("ac" / String)
        .and(warp::put())
        .and(warp::body::bytes())
        .map(move |key: String, body: bytes::Bytes| {
            let mut cache_lock = cache_put.lock().unwrap();
            cache_lock.insert(key.clone(), body.to_vec());
            println!("PUT /ac/{} - STORED ({} bytes)", key, body.len());
            warp::reply::with_status("OK", warp::http::StatusCode::OK)
        });

    // Statistics endpoint
    let cache_stats = cache.clone();
    let stats_route = warp::path("stats").map(move || {
        let cache_lock = cache_stats.lock().unwrap();
        let count = cache_lock.len();
        let total_bytes: usize = cache_lock.values().map(|v| v.len()).sum();

        format!(
            "Cache Statistics:\n  Entries: {}\n  Total size: {} bytes\n",
            count, total_bytes
        )
    });

    let routes = health.or(get_route).or(put_route).or(stats_route);

    let addr: SocketAddr = ([127, 0, 0, 1], 8080).into();
    println!("🚀 Rush HTTP Cache Server");
    println!("   Listening on: http://{}", addr);
    println!("   Health: http://{}/health", addr);
    println!("   Stats: http://{}/stats", addr);
    println!();
    println!("Ready to serve remote cache requests!");

    warp::serve(routes).run(addr).await;
}
