use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};

// Todo: some parts of this code, notably around ranges are never used but are here to prepare
// the migration of the parts of the codebase that still use untyped trees. At one point, this
// migration should be done or these functions deleted.

use super::{
	DbResult, DecodeError, Error, Result, Transaction, Tree, TxOpError, TxOpResult, TxValueIter,
	ValueIter,
};

pub use super::Db;

pub trait DbBytes: Sized {
	fn encode(&self) -> Vec<u8>;
	fn decode(bytes: &[u8]) -> std::result::Result<Self, DecodeError>;
}

/// Subtrait of [`DbBytes`] for types used as tree keys with operations where order matters
/// (`get_gt`, range, etc...).
///
/// Implementors must guarantee that the byte encoding is order-preserving:
/// for any `a, b: Self`, `a.cmp(&b) == a.encode().cmp(&b.encode())`.
pub trait DbOrdKey: DbBytes + Ord {}

#[derive(Clone)]
pub struct TypedTree<K, V> {
	inner: Tree,
	_phantom: PhantomData<[(K, V)]>,
}

impl<K: DbBytes, V: DbBytes> TypedTree<K, V> {
	pub fn new(tree: Tree) -> Self {
		Self {
			inner: tree,
			_phantom: PhantomData,
		}
	}

	pub fn db(&self) -> Db {
		self.inner.db()
	}

	pub fn untyped(&self) -> &Tree {
		&self.inner
	}

	pub fn get(&self, key: &K) -> Result<Option<V>> {
		self.inner
			.get(key.encode())?
			.map(|v| V::decode(&v).map_err(Error::from))
			.transpose()
	}

	pub fn approximate_len(&self) -> DbResult<usize> {
		self.inner.approximate_len()
	}

	pub fn is_empty(&self) -> DbResult<bool> {
		self.inner.is_empty()
	}

	pub fn insert(&self, key: &K, value: &V) -> DbResult<()> {
		self.inner.insert(key.encode(), value.encode())
	}

	pub fn remove(&self, key: &K) -> DbResult<()> {
		self.inner.remove(key.encode())
	}

	pub fn clear(&self) -> DbResult<()> {
		self.inner.clear()
	}

	pub fn tx_get(&self, tx: &Transaction<'_>, key: &K) -> TxOpResult<Option<V>> {
		tx.get(&self.inner, key.encode())?
			.map(|v| V::decode(&v).map_err(TxOpError::from))
			.transpose()
	}

	pub fn tx_insert(&self, tx: &mut Transaction<'_>, key: &K, value: &V) -> TxOpResult<()> {
		tx.insert(&self.inner, key.encode(), value.encode())
	}

	pub fn tx_remove(&self, tx: &mut Transaction<'_>, key: &K) -> TxOpResult<()> {
		tx.remove(&self.inner, key.encode())
	}

	pub fn tx_clear(&self, tx: &mut Transaction<'_>) -> TxOpResult<()> {
		tx.clear(&self.inner)
	}
}

impl<K: DbOrdKey, V: DbBytes> TypedTree<K, V> {
	pub fn first(&self) -> Result<Option<(K, V)>> {
		self.iter()?.next().transpose()
	}

	pub fn get_gt(&self, from: &K) -> Result<Option<(K, V)>> {
		self.inner
			.get_gt(from.encode())?
			.map(|(k, v)| {
				Ok((
					K::decode(&k).map_err(Error::from)?,
					V::decode(&v).map_err(Error::from)?,
				))
			})
			.transpose()
	}

	pub fn iter(&self) -> Result<TypedIter<'_, K, V>> {
		Ok(TypedIter::new(self.inner.iter()?))
	}

	pub fn iter_rev(&self) -> Result<TypedIter<'_, K, V>> {
		Ok(TypedIter::new(self.inner.iter_rev()?))
	}

	pub fn range<R: RangeBounds<K>>(&self, range: R) -> Result<TypedIter<'_, K, V>> {
		Ok(TypedIter::new(self.inner.range(encode_range(range))?))
	}

	pub fn range_rev<R: RangeBounds<K>>(&self, range: R) -> Result<TypedIter<'_, K, V>> {
		Ok(TypedIter::new(self.inner.range_rev(encode_range(range))?))
	}

	pub fn tx_iter<'t>(&self, tx: &'t Transaction<'_>) -> TxOpResult<TypedTxIter<'t, K, V>> {
		Ok(TypedTxIter::new(tx.iter(&self.inner)?))
	}

	pub fn tx_iter_rev<'t>(&self, tx: &'t Transaction<'_>) -> TxOpResult<TypedTxIter<'t, K, V>> {
		Ok(TypedTxIter::new(tx.iter_rev(&self.inner)?))
	}

	pub fn tx_range<'t, R: RangeBounds<K>>(
		&self,
		tx: &'t Transaction<'_>,
		range: R,
	) -> TxOpResult<TypedTxIter<'t, K, V>> {
		Ok(TypedTxIter::new(
			tx.range(&self.inner, encode_range(range))?,
		))
	}

	pub fn tx_range_rev<'t, R: RangeBounds<K>>(
		&self,
		tx: &'t Transaction<'_>,
		range: R,
	) -> TxOpResult<TypedTxIter<'t, K, V>> {
		Ok(TypedTxIter::new(
			tx.range_rev(&self.inner, encode_range(range))?,
		))
	}
}

impl<K: DbBytes, V: DbBytes> From<Tree> for TypedTree<K, V> {
	fn from(tree: Tree) -> Self {
		Self::new(tree)
	}
}

impl Db {
	pub fn open_typed_tree<K: DbBytes, V: DbBytes, S: AsRef<str>>(
		&self,
		name: S,
	) -> DbResult<TypedTree<K, V>> {
		Ok(TypedTree::new(self.open_tree(name)?))
	}
}

pub struct TypedIter<'a, K, V> {
	inner: ValueIter<'a>,
	_phantom: PhantomData<(K, V)>,
}

impl<'a, K, V> TypedIter<'a, K, V> {
	fn new(inner: ValueIter<'a>) -> Self {
		Self {
			inner,
			_phantom: PhantomData,
		}
	}
}

impl<K: DbOrdKey, V: DbBytes> Iterator for TypedIter<'_, K, V> {
	type Item = Result<(K, V)>;

	fn next(&mut self) -> Option<Self::Item> {
		self.inner.next().map(|res| {
			let (k, v) = res?;
			Ok((
				K::decode(&k).map_err(Error::from)?,
				V::decode(&v).map_err(Error::from)?,
			))
		})
	}
}

pub struct TypedTxIter<'a, K, V> {
	inner: TxValueIter<'a>,
	_phantom: PhantomData<(K, V)>,
}

impl<'a, K, V> TypedTxIter<'a, K, V> {
	fn new(inner: TxValueIter<'a>) -> Self {
		Self {
			inner,
			_phantom: PhantomData,
		}
	}
}

impl<K: DbOrdKey, V: DbBytes> Iterator for TypedTxIter<'_, K, V> {
	type Item = TxOpResult<(K, V)>;

	fn next(&mut self) -> Option<Self::Item> {
		self.inner.next().map(|res| {
			let (k, v) = res?;
			Ok((
				K::decode(&k).map_err(TxOpError::from)?,
				V::decode(&v).map_err(TxOpError::from)?,
			))
		})
	}
}

fn encode_range<K: DbOrdKey, R: RangeBounds<K>>(range: R) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
	(
		encode_bound(range.start_bound()),
		encode_bound(range.end_bound()),
	)
}

fn encode_bound<K: DbOrdKey>(bound: Bound<&K>) -> Bound<Vec<u8>> {
	match bound {
		Bound::Included(k) => Bound::Included(k.encode()),
		Bound::Excluded(k) => Bound::Excluded(k.encode()),
		Bound::Unbounded => Bound::Unbounded,
	}
}
