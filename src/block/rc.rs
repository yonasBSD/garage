use std::convert::TryInto;

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
		let old_rc = self.rc_table.tx_get(tx, hash)?.unwrap_or(RcEntry::Absent);
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
		let new_rc = self.rc_table.tx_get(tx, hash)?.unwrap_or(RcEntry::Absent).decrement();
		match new_rc {
			RcEntry::Absent => self.rc_table.tx_remove(tx, hash)?,
			_ => self.rc_table.tx_insert(tx, hash, &new_rc)?,
		}
		Ok(matches!(new_rc, RcEntry::Deletable { .. }))
	}

	/// Read a block's reference count
	pub(crate) fn get_block_rc(&self, hash: &Hash) -> Result<RcEntry, Error> {
		Ok(self.rc_table.get(hash)?.unwrap_or(RcEntry::Absent))
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
			let rcval = self.rc_table.tx_get(tx, hash)?.unwrap_or(RcEntry::Absent);
			if let RcEntry::Deletable { at_time } = rcval {
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
					let old_rc = self.rc_table.tx_get(tx, hash)?.unwrap_or(RcEntry::Absent);
					trace!(
						"Block RC for {:?}: stored={}, calculated={}",
						hash,
						old_rc.as_u64(),
						cnt
					);
					if cnt as u64 != old_rc.as_u64() {
						warn!(
							"Fixing inconsistent block RC for {:?}: was {}, should be {}",
							hash,
							old_rc.as_u64(),
							cnt
						);
						let new_rc = if cnt > 0 {
							RcEntry::Present { count: cnt as u64 }
						} else {
							RcEntry::Deletable {
								at_time: now_msec() + BLOCK_GC_DELAY.as_millis() as u64,
							}
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
			RcEntry::Present { count } => u64::to_be_bytes(*count).to_vec(),
			RcEntry::Deletable { at_time } => {
				[u64::to_be_bytes(0), u64::to_be_bytes(*at_time)].concat()
			}
			RcEntry::Absent => panic!("cannot encode RcEntry::Absent"),
		}
	}

	fn decode(bytes: &[u8]) -> db::Result<Self> {
		if bytes.len() == 8 {
			Ok(RcEntry::Present {
				count: u64::from_be_bytes(bytes.try_into().unwrap()),
			})
		} else if bytes.len() == 16 {
			Ok(RcEntry::Deletable {
				at_time: u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
			})
		} else {
			Err(db::Error::Decode(
				format!(
					"invalid RC entry: expected 8 or 16 bytes, got {}",
					bytes.len()
				)
				.into(),
			))
		}
	}
}

/// Describes the state of the reference counter for a block
#[derive(Clone, Copy, Debug)]
pub(crate) enum RcEntry {
	/// Present: the block has `count` references, with `count` > 0.
	///
	/// This is stored as `u64::to_be_bytes(count)`
	Present { count: u64 },

	/// Deletable: the block has zero references, and can be deleted
	/// once time (returned by `now_msec`) is larger than `at_time`
	/// (in millis since Unix epoch)
	///
	/// This is stored as [0u8; 8] followed by `u64::to_be_bytes(at_time)`,
	/// (this allows for the data format to be backwards compatible with
	/// previous Garage versions that didn't have this intermediate state)
	Deletable { at_time: u64 },

	/// Absent: the block has zero references, and can be deleted
	/// immediately
	Absent,
}

impl RcEntry {
	fn increment(self) -> Self {
		let old_count = match self {
			RcEntry::Present { count } => count,
			_ => 0,
		};
		RcEntry::Present {
			count: old_count + 1,
		}
	}

	fn decrement(self) -> Self {
		match self {
			RcEntry::Present { count } => {
				if count > 1 {
					RcEntry::Present { count: count - 1 }
				} else {
					RcEntry::Deletable {
						at_time: now_msec() + BLOCK_GC_DELAY.as_millis() as u64,
					}
				}
			}
			del => del,
		}
	}

	pub(crate) fn is_zero(&self) -> bool {
		matches!(self, RcEntry::Deletable { .. } | RcEntry::Absent)
	}

	pub(crate) fn is_nonzero(&self) -> bool {
		!self.is_zero()
	}

	pub(crate) fn is_deletable(&self) -> bool {
		match self {
			RcEntry::Present { .. } => false,
			RcEntry::Deletable { at_time } => now_msec() > *at_time,
			RcEntry::Absent => true,
		}
	}

	pub(crate) fn is_needed(&self) -> bool {
		!self.is_deletable()
	}

	pub(crate) fn as_u64(&self) -> u64 {
		match self {
			RcEntry::Present { count } => *count,
			_ => 0,
		}
	}
}
