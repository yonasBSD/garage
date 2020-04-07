use std::net::SocketAddr;
use std::sync::Arc;

use bytes::IntoBuf;
use hyper::service::{make_service_fn, service_fn};
use hyper::server::conn::AddrStream;
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use futures::future::Future;

use crate::error::Error;
use crate::proto::Message;
use crate::membership::System;

fn err_to_msg(x: Result<Message, Error>) -> Message {
	match x {
		Err(e) => Message::Error(format!("{}", e)),
		Ok(msg) => msg,
	}
}

async fn handler(sys: Arc<System>, req: Request<Body>, addr: SocketAddr) -> Result<Response<Body>, Error> {
	if req.method() != &Method::POST {
		let mut bad_request = Response::default();
		*bad_request.status_mut() = StatusCode::BAD_REQUEST;
		return Ok(bad_request);
	}

	let whole_body = hyper::body::to_bytes(req.into_body()).await?;
	let msg = rmp_serde::decode::from_read::<_, Message>(whole_body.into_buf())?;

	eprintln!("RPC from {}: {:?}", addr, msg);

	let resp = err_to_msg(match &msg {
		Message::Ping(ping) => sys.handle_ping(&addr, ping).await,
		Message::PullStatus => sys.handle_pull_status().await,
		Message::PullConfig => sys.handle_pull_config().await,
		Message::AdvertiseNodesUp(adv) => sys.handle_advertise_nodes_up(adv).await,
		Message::AdvertiseConfig(adv) => sys.handle_advertise_config(adv).await,

		_ => Ok(Message::Error(format!("Unexpected message: {:?}", msg))),
	});

	Ok(Response::new(Body::from(
		rmp_serde::encode::to_vec_named(&resp)?
        )))
}


pub async fn run_rpc_server(sys: Arc<System>, shutdown_signal: impl Future<Output=()>) -> Result<(), hyper::Error> {
    let bind_addr = ([0, 0, 0, 0], sys.config.rpc_port).into();

    let service = make_service_fn(|conn: &AddrStream| {
		let client_addr = conn.remote_addr();
		let sys = sys.clone();
		async move {
			Ok::<_, Error>(service_fn(move |req: Request<Body>| {
				let sys = sys.clone();
				handler(sys, req, client_addr)
			}))
		}
	});

    let server = Server::bind(&bind_addr).serve(service) ;

	let graceful = server.with_graceful_shutdown(shutdown_signal);
    println!("RPC server listening on http://{}", bind_addr);

	graceful.await
}