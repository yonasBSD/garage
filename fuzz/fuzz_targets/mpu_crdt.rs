#![no_main]

use garage_model::s3::mpu_table::{MpuPart, MpuPartKey, MultipartUpload};
use garage_table::crdt::Crdt;
use libfuzzer_sys::fuzz_target;

/// Build a MultipartUpload from an arbitrary deleted flag and parts list, using a fixed
/// upload_id/bucket_id/key so that CRDT state can be compared across merge results.
/// Duplicate part keys are dropped before construction.
/// `MpuPart.version` is fixed to a constant since it is identity data, not CRDT state:
/// two replicas of the same part (same MpuPartKey) always share the same version UUID.
/// If deleted, parts are cleared to ensure a valid initial CRDT state.
fn make_mpu(deleted: bool, mut parts: Vec<(MpuPartKey, MpuPart)>) -> MultipartUpload {
	parts.sort_by_key(|(k, _)| *k);
	parts.dedup_by_key(|(k, _)| *k);
	let mut mpu = MultipartUpload::new(
		[0u8; 32].into(),
		0,
		[0u8; 32].into(),
		String::new(),
		deleted,
	);
	for (key, mut part) in parts {
		part.version = [0u8; 32].into();
		mpu.parts.put(key, part);
	}
	if mpu.deleted.get() {
		mpu.parts.clear();
	}
	mpu
}

fn crdt_state(mpu: &MultipartUpload) -> (bool, &[(MpuPartKey, MpuPart)]) {
	(mpu.deleted.get(), mpu.parts.items())
}

fuzz_target!(|inputs: (
	(bool, Vec<(MpuPartKey, MpuPart)>),
	(bool, Vec<(MpuPartKey, MpuPart)>),
	(bool, Vec<(MpuPartKey, MpuPart)>)
)| {
	let ((d1, p1), (d2, p2), (d3, p3)) = inputs;
	let a = make_mpu(d1, p1);
	let b = make_mpu(d2, p2);
	let c = make_mpu(d3, p3);

	// Idempotency: merge(a, a) == a
	{
		let mut a2 = a.clone();
		a2.merge(&a.clone());
		assert_eq!(
			crdt_state(&a2),
			crdt_state(&a),
			"merge is not idempotent: {a2:#?} != {a:#?}"
		);
	}

	// Commutativity: crdt_state(merge(a, b)) == crdt_state(merge(b, a))
	let ab = {
		let mut t = a.clone();
		t.merge(&b);
		t
	};
	let ba = {
		let mut t = b.clone();
		t.merge(&a);
		t
	};
	assert_eq!(
		crdt_state(&ab),
		crdt_state(&ba),
		"merge is not commutative: {ab:#?} != {ba:#?}"
	);

	// Associativity: crdt_state(merge(merge(a, b), c)) == crdt_state(merge(a, merge(b, c)))
	let ab_c = {
		let mut t = ab.clone();
		t.merge(&c);
		t
	};
	let bc = {
		let mut t = b.clone();
		t.merge(&c);
		t
	};
	let a_bc = {
		let mut t = a.clone();
		t.merge(&bc);
		t
	};
	assert_eq!(
		crdt_state(&ab_c),
		crdt_state(&a_bc),
		"merge is not associative: {ab_c:#?} != {a_bc:#?}"
	);
});
