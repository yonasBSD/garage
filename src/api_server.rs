use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::future::Future;
use futures::stream::*;
use hyper::body::{Bytes, HttpBody};
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};

use crate::data;
use crate::data::*;
use crate::error::Error;
use crate::http_util::*;
use crate::proto::*;
use crate::rpc_client::*;
use crate::server::Garage;
use crate::table::EmptySortKey;

type BodyType = Box<dyn HttpBody<Data = Bytes, Error = Error> + Send + Unpin>;

pub async fn run_api_server(
	garage: Arc<Garage>,
	shutdown_signal: impl Future<Output = ()>,
) -> Result<(), Error> {
	let addr = ([0, 0, 0, 0, 0, 0, 0, 0], garage.system.config.api_port).into();

	let service = make_service_fn(|conn: &AddrStream| {
		let garage = garage.clone();
		let client_addr = conn.remote_addr();
		async move {
			Ok::<_, Error>(service_fn(move |req: Request<Body>| {
				let garage = garage.clone();
				handler(garage, req, client_addr)
			}))
		}
	});

	let server = Server::bind(&addr).serve(service);

	let graceful = server.with_graceful_shutdown(shutdown_signal);
	println!("API server listening on http://{}", addr);

	graceful.await?;
	Ok(())
}

async fn handler(
	garage: Arc<Garage>,
	req: Request<Body>,
	addr: SocketAddr,
) -> Result<Response<BodyType>, Error> {
	match handler_inner(garage, req, addr).await {
		Ok(x) => Ok(x),
		Err(e) => {
			let body: BodyType = Box::new(BytesBody::from(format!("{}\n", e)));
			let mut http_error = Response::new(body);
			*http_error.status_mut() = e.http_status_code();
			Ok(http_error)
		}
	}
}

async fn handler_inner(
	garage: Arc<Garage>,
	req: Request<Body>,
	addr: SocketAddr,
) -> Result<Response<BodyType>, Error> {
	eprintln!("{} {} {}", addr, req.method(), req.uri());

	let bucket = req
		.headers()
		.get(hyper::header::HOST)
		.map(|x| x.to_str().map_err(Error::from))
		.unwrap_or(Err(Error::BadRequest(format!("Host: header missing"))))?
		.to_lowercase();
	let key = req.uri().path().to_string();

	match req.method() {
		&Method::GET => Ok(handle_get(garage, &bucket, &key).await?),
		&Method::PUT => {
			let mime_type = req
				.headers()
				.get(hyper::header::CONTENT_TYPE)
				.map(|x| x.to_str())
				.unwrap_or(Ok("blob"))?
				.to_string();
			let version_uuid =
				handle_put(garage, &mime_type, &bucket, &key, req.into_body()).await?;
			Ok(Response::new(Box::new(BytesBody::from(format!(
				"{:?}\n",
				version_uuid
			)))))
		}
		&Method::DELETE => {
			let version_uuid = handle_delete(garage, &bucket, &key).await?;
			Ok(Response::new(Box::new(BytesBody::from(format!(
				"{:?}\n",
				version_uuid
			)))))
		}
		_ => Err(Error::BadRequest(format!("Invalid method"))),
	}
}

async fn handle_put(
	garage: Arc<Garage>,
	mime_type: &str,
	bucket: &str,
	key: &str,
	body: Body,
) -> Result<UUID, Error> {
	let version_uuid = gen_uuid();

	let mut chunker = BodyChunker::new(body, garage.system.config.block_size);
	let first_block = match chunker.next().await? {
		Some(x) => x,
		None => return Err(Error::BadRequest(format!("Empty body"))),
	};

	let mut object = Object {
		bucket: bucket.into(),
		key: key.into(),
		versions: Vec::new(),
	};
	object.versions.push(Box::new(ObjectVersion {
		uuid: version_uuid.clone(),
		timestamp: now_msec(),
		mime_type: mime_type.to_string(),
		size: first_block.len() as u64,
		is_complete: false,
		data: ObjectVersionData::DeleteMarker,
	}));

	if first_block.len() < INLINE_THRESHOLD {
		object.versions[0].data = ObjectVersionData::Inline(first_block);
		object.versions[0].is_complete = true;
		garage.object_table.insert(&object).await?;
		return Ok(version_uuid);
	}

	let version = Version {
		uuid: version_uuid.clone(),
		deleted: false,
		blocks: Vec::new(),
		bucket: bucket.into(),
		key: key.into(),
	};

	let first_block_hash = hash(&first_block[..]);
	object.versions[0].data = ObjectVersionData::FirstBlock(first_block_hash.clone());
	garage.object_table.insert(&object).await?;

	let mut next_offset = first_block.len();
	let mut put_curr_version_block =
		put_block_meta(garage.clone(), &version, 0, first_block_hash.clone());
	let mut put_curr_block = put_block(garage.clone(), first_block_hash, first_block);

	loop {
		let (_, _, next_block) =
			futures::try_join!(put_curr_block, put_curr_version_block, chunker.next())?;
		if let Some(block) = next_block {
			let block_hash = hash(&block[..]);
			let block_len = block.len();
			put_curr_version_block = put_block_meta(
				garage.clone(),
				&version,
				next_offset as u64,
				block_hash.clone(),
			);
			put_curr_block = put_block(garage.clone(), block_hash, block);
			next_offset += block_len;
		} else {
			break;
		}
	}

	// TODO: if at any step we have an error, we should undo everything we did

	object.versions[0].is_complete = true;
	object.versions[0].size = next_offset as u64;
	garage.object_table.insert(&object).await?;
	Ok(version_uuid)
}

async fn put_block_meta(
	garage: Arc<Garage>,
	version: &Version,
	offset: u64,
	hash: Hash,
) -> Result<(), Error> {
	let mut version = version.clone();
	version.blocks.push(VersionBlock {
		offset,
		hash: hash.clone(),
	});

	let block_ref = BlockRef {
		block: hash,
		version: version.uuid.clone(),
		deleted: false,
	};

	futures::try_join!(
		garage.version_table.insert(&version),
		garage.block_ref_table.insert(&block_ref),
	)?;
	Ok(())
}

async fn put_block(garage: Arc<Garage>, hash: Hash, data: Vec<u8>) -> Result<(), Error> {
	let who = garage
		.system
		.ring
		.borrow()
		.clone()
		.walk_ring(&hash, garage.system.config.data_replication_factor);
	rpc_try_call_many(
		garage.system.clone(),
		&who[..],
		&Message::PutBlock(PutBlockMessage { hash, data }),
		(garage.system.config.data_replication_factor + 1) / 2,
		DEFAULT_TIMEOUT,
	)
	.await?;
	Ok(())
}

struct BodyChunker {
	body: Body,
	read_all: bool,
	block_size: usize,
	buf: VecDeque<u8>,
}

impl BodyChunker {
	fn new(body: Body, block_size: usize) -> Self {
		Self {
			body,
			read_all: false,
			block_size,
			buf: VecDeque::new(),
		}
	}
	async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
		while !self.read_all && self.buf.len() < self.block_size {
			if let Some(block) = self.body.next().await {
				let bytes = block?;
				eprintln!("Body next: {} bytes", bytes.len());
				self.buf.extend(&bytes[..]);
			} else {
				self.read_all = true;
			}
		}
		if self.buf.len() == 0 {
			Ok(None)
		} else if self.buf.len() <= self.block_size {
			let block = self.buf.drain(..).collect::<Vec<u8>>();
			Ok(Some(block))
		} else {
			let block = self.buf.drain(..self.block_size).collect::<Vec<u8>>();
			Ok(Some(block))
		}
	}
}

async fn handle_delete(garage: Arc<Garage>, bucket: &str, key: &str) -> Result<UUID, Error> {
	let version_uuid = gen_uuid();

	let mut object = Object {
		bucket: bucket.into(),
		key: key.into(),
		versions: Vec::new(),
	};
	object.versions.push(Box::new(ObjectVersion {
		uuid: version_uuid.clone(),
		timestamp: now_msec(),
		mime_type: "application/x-delete-marker".into(),
		size: 0,
		is_complete: true,
		data: ObjectVersionData::DeleteMarker,
	}));

	garage.object_table.insert(&object).await?;
	return Ok(version_uuid);
}

async fn handle_get(
	garage: Arc<Garage>,
	bucket: &str,
	key: &str,
) -> Result<Response<BodyType>, Error> {
	let mut object = match garage
		.object_table
		.get(&bucket.to_string(), &key.to_string())
		.await?
	{
		None => return Err(Error::NotFound),
		Some(o) => o,
	};

	let last_v = match object
		.versions
		.drain(..)
		.rev()
		.filter(|v| v.is_complete)
		.next()
	{
		Some(v) => v,
		None => return Err(Error::NotFound),
	};

	let resp_builder = Response::builder()
		.header("Content-Type", last_v.mime_type)
		.status(StatusCode::OK);

	match last_v.data {
		ObjectVersionData::DeleteMarker => Err(Error::NotFound),
		ObjectVersionData::Inline(bytes) => {
			let body: BodyType = Box::new(BytesBody::from(bytes));
			Ok(resp_builder.body(body)?)
		}
		ObjectVersionData::FirstBlock(first_block_hash) => {
			let read_first_block = get_block(garage.clone(), &first_block_hash);
			let get_next_blocks = garage.version_table.get(&last_v.uuid, &EmptySortKey);

			let (first_block, version) = futures::try_join!(read_first_block, get_next_blocks)?;
			let version = match version {
				Some(v) => v,
				None => return Err(Error::NotFound),
			};

			let mut blocks = version
				.blocks
				.iter()
				.map(|vb| (vb.hash.clone(), None))
				.collect::<Vec<_>>();
			blocks[0].1 = Some(first_block);

			let body_stream = futures::stream::iter(blocks)
				.map(move |(hash, data_opt)| {
					let garage = garage.clone();
					async move {
						if let Some(data) = data_opt {
							Ok(Bytes::from(data))
						} else {
							get_block(garage.clone(), &hash).await.map(Bytes::from)
						}
					}
				})
				.buffered(2);
			let body: BodyType = Box::new(StreamBody::new(Box::pin(body_stream)));
			Ok(resp_builder.body(body)?)
		}
	}
}

async fn get_block(garage: Arc<Garage>, hash: &Hash) -> Result<Vec<u8>, Error> {
	let who = garage
		.system
		.ring
		.borrow()
		.clone()
		.walk_ring(&hash, garage.system.config.data_replication_factor);
	let resps = rpc_try_call_many(
		garage.system.clone(),
		&who[..],
		&Message::GetBlock(hash.clone()),
		1,
		DEFAULT_TIMEOUT,
	)
	.await?;

	for resp in resps {
		if let Message::PutBlock(pbm) = resp {
			if data::hash(&pbm.data) == *hash {
				return Ok(pbm.data);
			}
		}
	}
	Err(Error::Message(format!("No valid blocks returned")))
}
