use std::convert::TryInto;
use std::num::NonZeroU64;

use arc_swap::ArcSwapOption;

use garage_db as db;

use garage_util::data::*;
use garage_util::error::*;
use garage_util::time::*;

use crate::manager::BLOCK_GC_DELAY;

pub type CalculateRefcount =
	Box<dyn Fn(&db::Transaction, &Hash) -> db::TxResult<usize, Error> + Send + Sync>;

pub struct BlockRc {
	pub(crate) rc_table: db::TypedTree<Hash, RcEntry>,
	pub(crate) recalc_rc: ArcSwapOption<Vec<CalculateRefcount>>,
}

impl BlockRc {
	pub(crate) fn new(rc: db::TypedTree<Hash, RcEntry>) -> Self {
		Self {
			rc_table: rc,
			recalc_rc: ArcSwapOption::new(None),
		}
	}

	/// Increment the reference counter associated to a hash.
	/// Returns true if the RC goes from zero to nonzero.
	pub(crate) fn block_incref(
		&self,
		tx: &mut db::Transaction,
		hash: &Hash,
	) -> db::TxOpResult<bool> {
		let old_rc = RcState(self.rc_table.tx_get(tx, hash)?);
		self.rc_table.tx_insert(tx, hash, &old_rc.increment())?;
		Ok(old_rc.is_zero())
	}

	/// Decrement the reference counter associated to a hash.
	/// Returns true if the RC is now zero.
	pub(crate) fn block_decref(
		&self,
		tx: &mut db::Transaction,
		hash: &Hash,
	) -> db::TxOpResult<bool> {
		let new_rc = RcState(self.rc_table.tx_get(tx, hash)?).decrement();
		match &new_rc.0 {
			None => self.rc_table.tx_remove(tx, hash)?,
			Some(rc) => self.rc_table.tx_insert(tx, hash, rc)?,
		}
		Ok(matches!(new_rc.0, Some(RcEntry::Deletable { .. })))
	}

	/// Read a block's reference counting state
	pub(crate) fn get_block_rc(&self, hash: &Hash) -> Result<RcState, Error> {
		Ok(RcState(self.rc_table.get(hash)?))
	}

	/// Return the first hash stored in the RC table at or after `cursor`
	pub fn get_first_hash_from(&self, cursor: Hash) -> Result<Option<Hash>, Error> {
		Ok(self
			.rc_table
			.range(cursor..)?
			.next()
			.transpose()?
			.map(|(k, _)| k))
	}

	/// Delete an entry in the RC table if it is deletable and the
	/// deletion time has passed
	pub(crate) fn clear_deleted_block_rc(&self, hash: &Hash) -> Result<(), Error> {
		let now = now_msec();
		self.rc_table.db().transaction(|tx| {
			let rcval = self.rc_table.tx_get(tx, hash)?;
			if let Some(RcEntry::Deletable { at_time }) = rcval {
				if now > at_time {
					self.rc_table.tx_remove(tx, hash)?;
				}
			}
			Ok(())
		})?;
		Ok(())
	}

	/// Recalculate the reference counter of a block
	/// to fix potential inconsistencies
	pub fn recalculate_rc(&self, hash: &Hash) -> Result<(usize, bool), Error> {
		if let Some(recalc_fns) = self.recalc_rc.load().as_ref() {
			trace!("Repair block RC for {:?}", hash);
			let res = self
				.rc_table
				.db()
				.transaction(|tx| {
					let mut cnt = 0;
					for f in recalc_fns.iter() {
						cnt += f(tx, hash)?;
					}
					let old_count = RcState(self.rc_table.tx_get(tx, hash)?).as_u64();
					trace!(
						"Block RC for {:?}: stored={}, calculated={}",
						hash,
						old_count,
						cnt
					);
					if cnt as u64 != old_count {
						warn!(
							"Fixing inconsistent block RC for {:?}: was {}, should be {}",
							hash, old_count, cnt
						);
						let new_rc = match NonZeroU64::new(cnt as u64) {
							Some(count) => RcEntry::Present { count },
							None => RcEntry::Deletable {
								at_time: now_msec() + BLOCK_GC_DELAY.as_millis() as u64,
							},
						};
						self.rc_table.tx_insert(tx, hash, &new_rc)?;
						Ok((cnt, true))
					} else {
						Ok((cnt, false))
					}
				})
				.map_err(Error::from);
			if let Err(e) = &res {
				error!("Failed to fix RC for block {:?}: {}", hash, e);
			}
			res
		} else {
			Err(Error::Message(
				"Block RC recalculation is not available at this point".into(),
			))
		}
	}
}

impl db::DbBytes for RcEntry {
	fn encode(&self) -> Vec<u8> {
		match self {
			RcEntry::Present { count } => u64::to_be_bytes(count.get()).to_vec(),
			RcEntry::Deletable { at_time } => {
				[u64::to_be_bytes(0), u64::to_be_bytes(*at_time)].concat()
			}
		}
	}

	fn decode(bytes: &[u8]) -> std::result::Result<Self, db::DecodeError> {
		if bytes.len() == 8 {
			let count = NonZeroU64::new(u64::from_be_bytes(bytes.try_into().unwrap()))
				.ok_or(db::DecodeError("invalid RC entry: zero count".into()))?;
			Ok(RcEntry::Present { count })
		} else if bytes.len() == 16 {
			Ok(RcEntry::Deletable {
				at_time: u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
			})
		} else {
			Err(db::DecodeError(
				format!(
					"invalid RC entry: expected 8 or 16 bytes, got {}",
					bytes.len()
				)
				.into(),
			))
		}
	}
}

/// A block's entry in the RC table.
///
/// A block with zero references and no pending deletion has no entry
/// in the RC table at all: see [`RcState`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum RcEntry {
	/// Present: the block has `count` references.
	///
	/// This is stored as `u64::to_be_bytes(count)`
	Present { count: NonZeroU64 },

	/// Deletable: the block has zero references, and can be deleted
	/// once time (returned by `now_msec`) is larger than `at_time`
	/// (in millis since Unix epoch)
	///
	/// This is stored as [0u8; 8] followed by `u64::to_be_bytes(at_time)`,
	/// (this allows for the data format to be backwards compatible with
	/// previous Garage versions that didn't have this intermediate state)
	Deletable { at_time: u64 },
}

/// Describes the state of the reference counter for a block: the block's
/// entry in the RC table, or `None` if it has none, meaning the block has
/// zero references and can be deleted immediately.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RcState(Option<RcEntry>);

impl RcState {
	/// The new RC table entry after a reference is taken on the block
	fn increment(&self) -> RcEntry {
		let count = match self.0 {
			Some(RcEntry::Present { count }) => count.saturating_add(1),
			_ => NonZeroU64::new(1).unwrap(),
		};
		RcEntry::Present { count }
	}

	/// The new state after a reference to the block is dropped
	fn decrement(&self) -> Self {
		RcState(match self.0 {
			Some(RcEntry::Present { count }) => Some(match NonZeroU64::new(count.get() - 1) {
				Some(count) => RcEntry::Present { count },
				None => RcEntry::Deletable {
					at_time: now_msec() + BLOCK_GC_DELAY.as_millis() as u64,
				},
			}),
			unchanged => unchanged,
		})
	}

	pub(crate) fn is_zero(&self) -> bool {
		matches!(self.0, None | Some(RcEntry::Deletable { .. }))
	}

	pub(crate) fn is_nonzero(&self) -> bool {
		!self.is_zero()
	}

	pub(crate) fn is_deletable(&self) -> bool {
		match self.0 {
			Some(RcEntry::Present { .. }) => false,
			Some(RcEntry::Deletable { at_time }) => now_msec() > at_time,
			None => true,
		}
	}

	pub(crate) fn is_needed(&self) -> bool {
		!self.is_deletable()
	}

	pub(crate) fn as_u64(&self) -> u64 {
		match self.0 {
			Some(RcEntry::Present { count }) => count.get(),
			_ => 0,
		}
	}
}
