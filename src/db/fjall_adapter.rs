use core::ops::Bound;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use fjall::{
	PartitionCreateOptions, PersistMode, TransactionalKeyspace, TransactionalPartitionHandle,
	WriteTransaction,
};

use crate::{
	Db, Error, IDb, ITx, ITxFn, OnCommit, Result, TxError, TxFnResult, TxOpError, TxOpResult,
	TxResult, TxValueIter, Value, ValueIter,
};

pub use fjall;

// -- err

impl From<fjall::Error> for Error {
	fn from(e: fjall::Error) -> Error {
		Error(format!("fjall: {}", e).into())
	}
}

impl From<fjall::LsmError> for Error {
	fn from(e: fjall::LsmError) -> Error {
		Error(format!("fjall lsm_tree: {}", e).into())
	}
}

impl From<fjall::Error> for TxOpError {
	fn from(e: fjall::Error) -> TxOpError {
		TxOpError(e.into())
	}
}

// -- db

pub struct FjallDb {
	path: PathBuf,
	keyspace: TransactionalKeyspace,
	trees: RwLock<(Vec<TransactionalPartitionHandle>, HashMap<String, usize>)>,
}

type ByteRefRangeBound<'r> = (Bound<&'r [u8]>, Bound<&'r [u8]>);

impl FjallDb {
	pub fn init(path: &PathBuf, keyspace: TransactionalKeyspace) -> Db {
		let s = Self {
			path: path.clone(),
			keyspace,
			trees: RwLock::new((Vec::new(), HashMap::new())),
		};
		Db(Arc::new(s))
	}

	fn get_tree(&self, i: usize) -> Result<TransactionalPartitionHandle> {
		self.trees
			.read()
			.unwrap()
			.0
			.get(i)
			.cloned()
			.ok_or_else(|| Error("invalid tree id".into()))
	}

	fn canonicalize(name: &str) -> String {
		name.chars()
			.map(|c| {
				if c.is_alphanumeric() || c == '-' || c == '_' {
					c
				} else {
					'_'
				}
			})
			.collect::<String>()
	}
}

impl IDb for FjallDb {
	fn engine(&self) -> String {
		"LSM trees (using Fjall crate)".into()
	}

	fn open_tree(&self, name: &str) -> Result<usize> {
		let mut trees = self.trees.write().unwrap();
		let canonical_name = FjallDb::canonicalize(name);
		if let Some(i) = trees.1.get(&canonical_name) {
			Ok(*i)
		} else {
			let tree = self
				.keyspace
				.open_partition(&canonical_name, PartitionCreateOptions::default())?;
			let i = trees.0.len();
			trees.0.push(tree);
			trees.1.insert(canonical_name, i);
			Ok(i)
		}
	}

	fn list_trees(&self) -> Result<Vec<String>> {
		Ok(self
			.keyspace
			.list_partitions()
			.iter()
			.map(|n| n.to_string())
			.collect())
	}

	fn snapshot(&self, to: &PathBuf) -> Result<()> {
		std::fs::create_dir_all(to)?;
		let mut path = to.clone();
		path.push("data.fjall");

		let source_keyspace = fjall::Config::new(&self.path).open()?;
		let copy_keyspace = fjall::Config::new(path).open()?;

		for partition_name in source_keyspace.list_partitions() {
			let source_partition = source_keyspace
				.open_partition(&partition_name, PartitionCreateOptions::default())?;
			let snapshot = source_partition.snapshot();
			let copy_partition =
				copy_keyspace.open_partition(&partition_name, PartitionCreateOptions::default())?;

			for entry in snapshot.iter() {
				let (key, value) = entry?;
				copy_partition.insert(key, value)?;
			}
		}

		copy_keyspace.persist(PersistMode::SyncAll)?;
		Ok(())
	}

	// ----

	fn get(&self, tree_idx: usize, key: &[u8]) -> Result<Option<Value>> {
		let tree = self.get_tree(tree_idx)?;
		let tx = self.keyspace.read_tx();
		let val = tx.get(&tree, key)?;
		match val {
			None => Ok(None),
			Some(v) => Ok(Some(v.to_vec())),
		}
	}

	fn len(&self, tree_idx: usize) -> Result<usize> {
		let tree = self.get_tree(tree_idx)?;
		let tx = self.keyspace.read_tx();
		Ok(tx.len(&tree)?)
	}

	fn insert(&self, tree_idx: usize, key: &[u8], value: &[u8]) -> Result<()> {
		let tree = self.get_tree(tree_idx)?;
		let mut tx = self.keyspace.write_tx();
		tx.insert(&tree, key, value);
		tx.commit()?;
		Ok(())
	}

	fn remove(&self, tree_idx: usize, key: &[u8]) -> Result<()> {
		let tree = self.get_tree(tree_idx)?;
		let mut tx = self.keyspace.write_tx();
		tx.remove(&tree, key);
		tx.commit()?;
		Ok(())
	}

	fn clear(&self, tree_idx: usize) -> Result<()> {
		let tree = self.get_tree(tree_idx)?;
		let tree_name = tree.inner().name.clone();
		self.keyspace.delete_partition(tree)?;
		let tree = self
			.keyspace
			.open_partition(&tree_name, PartitionCreateOptions::default())?;
		let mut trees = self.trees.write().unwrap();
		trees.0[tree_idx] = tree;
		Ok(())
	}

	fn iter(&self, tree_idx: usize) -> Result<ValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?;
		let tx = self.keyspace.read_tx();
		Ok(Box::new(tx.iter(&tree).map(iterator_remap)))
	}

	fn iter_rev(&self, tree_idx: usize) -> Result<ValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?;
		let tx = self.keyspace.read_tx();
		Ok(Box::new(tx.iter(&tree).rev().map(iterator_remap)))
	}

	fn range<'r>(
		&self,
		tree_idx: usize,
		low: Bound<&'r [u8]>,
		high: Bound<&'r [u8]>,
	) -> Result<ValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?;
		let tx = self.keyspace.read_tx();
		Ok(Box::new(
			tx.range::<&'r [u8], ByteRefRangeBound>(&tree, (low, high))
				.map(iterator_remap),
		))
	}
	fn range_rev<'r>(
		&self,
		tree_idx: usize,
		low: Bound<&'r [u8]>,
		high: Bound<&'r [u8]>,
	) -> Result<ValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?;
		let tx = self.keyspace.read_tx();
		Ok(Box::new(
			tx.range::<&'r [u8], ByteRefRangeBound>(&tree, (low, high))
				.rev()
				.map(iterator_remap),
		))
	}

	// ----

	fn transaction(&self, f: &dyn ITxFn) -> TxResult<OnCommit, ()> {
		let trees = self.trees.read().unwrap();
		let mut tx = FjallTx {
			trees: &trees.0[..],
			tx: self.keyspace.write_tx(),
		};

		let res = f.try_on(&mut tx);
		match res {
			TxFnResult::Ok(on_commit) => {
				tx.tx.commit().map_err(Error::from).map_err(TxError::Db)?;
				Ok(on_commit)
			}
			TxFnResult::Abort => {
				tx.tx.rollback();
				Err(TxError::Abort(()))
			}
			TxFnResult::DbErr => {
				tx.tx.rollback();
				Err(TxError::Db(Error(
					"(this message will be discarded)".into(),
				)))
			}
		}
	}
}

// ----

struct FjallTx<'a> {
	trees: &'a [TransactionalPartitionHandle],
	tx: WriteTransaction<'a>,
}

impl<'a> FjallTx<'a> {
	fn get_tree(&self, i: usize) -> TxOpResult<&TransactionalPartitionHandle> {
		self.trees.get(i).ok_or_else(|| {
			TxOpError(Error(
				"invalid tree id (it might have been openned after the transaction started)".into(),
			))
		})
	}
}

impl<'a> ITx for FjallTx<'a> {
	fn get(&self, tree_idx: usize, key: &[u8]) -> TxOpResult<Option<Value>> {
		let tree = self.get_tree(tree_idx)?;
		match self.tx.get(tree, key)? {
			Some(v) => Ok(Some(v.to_vec())),
			None => Ok(None),
		}
	}
	fn len(&self, tree_idx: usize) -> TxOpResult<usize> {
		let tree = self.get_tree(tree_idx)?;
		Ok(self.tx.len(tree)? as usize)
	}

	fn insert(&mut self, tree_idx: usize, key: &[u8], value: &[u8]) -> TxOpResult<()> {
		let tree = self.get_tree(tree_idx)?.clone();
		self.tx.insert(&tree, key, value);
		Ok(())
	}
	fn remove(&mut self, tree_idx: usize, key: &[u8]) -> TxOpResult<()> {
		let tree = self.get_tree(tree_idx)?.clone();
		self.tx.remove(&tree, key);
		Ok(())
	}
	fn clear(&mut self, _tree_idx: usize) -> TxOpResult<()> {
		unimplemented!("LSM tree clearing in cross-partition transaction is not supported")
	}

	fn iter(&self, tree_idx: usize) -> TxOpResult<TxValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?.clone();
		Ok(Box::new(self.tx.iter(&tree).map(iterator_remap_tx)))
	}
	fn iter_rev(&self, tree_idx: usize) -> TxOpResult<TxValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?.clone();
		Ok(Box::new(self.tx.iter(&tree).rev().map(iterator_remap_tx)))
	}

	fn range<'r>(
		&self,
		tree_idx: usize,
		low: Bound<&'r [u8]>,
		high: Bound<&'r [u8]>,
	) -> TxOpResult<TxValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?;
		let low = clone_bound(low);
		let high = clone_bound(high);
		Ok(Box::new(
			self.tx
				.range::<Vec<u8>, ByteVecRangeBounds>(&tree, (low, high))
				.map(iterator_remap_tx),
		))
	}
	fn range_rev<'r>(
		&self,
		tree_idx: usize,
		low: Bound<&'r [u8]>,
		high: Bound<&'r [u8]>,
	) -> TxOpResult<TxValueIter<'_>> {
		let tree = self.get_tree(tree_idx)?;
		let low = clone_bound(low);
		let high = clone_bound(high);
		Ok(Box::new(
			self.tx
				.range::<Vec<u8>, ByteVecRangeBounds>(&tree, (low, high))
				.rev()
				.map(iterator_remap_tx),
		))
	}
}

// -- maps fjall's (k, v) to ours

fn iterator_remap(r: fjall::Result<(fjall::Slice, fjall::Slice)>) -> Result<(Value, Value)> {
	r.map(|(k, v)| (k.to_vec(), v.to_vec()))
		.map_err(|e| e.into())
}

fn iterator_remap_tx(r: fjall::Result<(fjall::Slice, fjall::Slice)>) -> TxOpResult<(Value, Value)> {
	r.map(|(k, v)| (k.to_vec(), v.to_vec()))
		.map_err(|e| e.into())
}

// -- utils to deal with Garage's tightness on Bound lifetimes

type ByteVecBound = Bound<Vec<u8>>;
type ByteVecRangeBounds = (ByteVecBound, ByteVecBound);

fn clone_bound(bound: Bound<&[u8]>) -> ByteVecBound {
	let value = match bound {
		Bound::Excluded(v) | Bound::Included(v) => v.to_vec(),
		Bound::Unbounded => vec![],
	};

	match bound {
		Bound::Included(_) => Bound::Included(value),
		Bound::Excluded(_) => Bound::Excluded(value),
		Bound::Unbounded => Bound::Unbounded,
	}
}
