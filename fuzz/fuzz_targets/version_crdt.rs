#![no_main]

use garage_model::s3::version_table::{Version, VersionBacklink, VersionBlock, VersionBlockKey};
use garage_table::crdt::Crdt;
use libfuzzer_sys::fuzz_target;

/// Build a Version from an arbitrary deleted flag and block list, using a fixed uuid/backlink
/// so that CRDT state can be compared across merge results.
/// Duplicate block keys are dropped before construction.
/// If deleted, blocks are cleared to ensure a valid initial CRDT state.
fn make_version(deleted: bool, mut blocks: Vec<(VersionBlockKey, VersionBlock)>) -> Version {
	blocks.sort_by_key(|(k, _)| *k);
	blocks.dedup_by_key(|(k, _)| *k);
	let mut v = Version::new(
		[0u8; 32].into(),
		VersionBacklink::Object {
			bucket_id: [0u8; 32].into(),
			key: String::new(),
		},
		deleted,
	);
	for (key, block) in blocks {
		v.blocks.put(key, block);
	}
	if v.deleted.get() {
		v.blocks.clear();
	}
	v
}

fn crdt_state(v: &Version) -> (bool, &[(VersionBlockKey, VersionBlock)]) {
	(v.deleted.get(), v.blocks.items())
}

fuzz_target!(|inputs: (
	(bool, Vec<(VersionBlockKey, VersionBlock)>),
	(bool, Vec<(VersionBlockKey, VersionBlock)>),
	(bool, Vec<(VersionBlockKey, VersionBlock)>)
)| {
	let ((d1, b1), (d2, b2), (d3, b3)) = inputs;
	let a = make_version(d1, b1);
	let b = make_version(d2, b2);
	let c = make_version(d3, b3);

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
