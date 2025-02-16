use std::convert::{TryFrom, TryInto};
use std::hash::Hasher;

use base64::prelude::*;
use crc32c::Crc32cHasher as Crc32c;
use crc32fast::Hasher as Crc32;
use md5::{Digest, Md5};
use sha1::Sha1;
use sha2::Sha256;

use http::HeaderName;

use garage_util::data::*;

use garage_model::s3::object_table::{ChecksumAlgorithm, ChecksumValue};

use super::error::*;

pub const X_AMZ_CHECKSUM_ALGORITHM: HeaderName =
	HeaderName::from_static("x-amz-checksum-algorithm");
pub const X_AMZ_CHECKSUM_MODE: HeaderName = HeaderName::from_static("x-amz-checksum-mode");
pub const X_AMZ_CHECKSUM_CRC32: HeaderName = HeaderName::from_static("x-amz-checksum-crc32");
pub const X_AMZ_CHECKSUM_CRC32C: HeaderName = HeaderName::from_static("x-amz-checksum-crc32c");
pub const X_AMZ_CHECKSUM_SHA1: HeaderName = HeaderName::from_static("x-amz-checksum-sha1");
pub const X_AMZ_CHECKSUM_SHA256: HeaderName = HeaderName::from_static("x-amz-checksum-sha256");

pub type Crc32Checksum = [u8; 4];
pub type Crc32cChecksum = [u8; 4];
pub type Md5Checksum = [u8; 16];
pub type Sha1Checksum = [u8; 20];
pub type Sha256Checksum = [u8; 32];

#[derive(Debug, Default)]
pub struct ExpectedChecksums {
	// base64-encoded md5 (content-md5 header)
	pub md5: Option<String>,
	// content_sha256 (as a Hash / FixedBytes32)
	pub sha256: Option<Hash>,
	// extra x-amz-checksum-* header
	pub extra: Option<ChecksumValue>,
}

pub struct Checksummer {
	pub crc32: Option<Crc32>,
	pub crc32c: Option<Crc32c>,
	pub md5: Option<Md5>,
	pub sha1: Option<Sha1>,
	pub sha256: Option<Sha256>,
}

#[derive(Default)]
pub struct Checksums {
	pub crc32: Option<Crc32Checksum>,
	pub crc32c: Option<Crc32cChecksum>,
	pub md5: Option<Md5Checksum>,
	pub sha1: Option<Sha1Checksum>,
	pub sha256: Option<Sha256Checksum>,
}

impl Checksummer {
	pub fn init(expected: &ExpectedChecksums, require_md5: bool) -> Self {
		let mut ret = Self {
			crc32: None,
			crc32c: None,
			md5: None,
			sha1: None,
			sha256: None,
		};

		if expected.md5.is_some() || require_md5 {
			ret.md5 = Some(Md5::new());
		}
		if expected.sha256.is_some() || matches!(&expected.extra, Some(ChecksumValue::Sha256(_))) {
			ret.sha256 = Some(Sha256::new());
		}
		if matches!(&expected.extra, Some(ChecksumValue::Crc32(_))) {
			ret.crc32 = Some(Crc32::new());
		}
		if matches!(&expected.extra, Some(ChecksumValue::Crc32c(_))) {
			ret.crc32c = Some(Crc32c::default());
		}
		if matches!(&expected.extra, Some(ChecksumValue::Sha1(_))) {
			ret.sha1 = Some(Sha1::new());
		}
		ret
	}

	pub fn add(mut self, algo: Option<ChecksumAlgorithm>) -> Self {
		match algo {
			Some(ChecksumAlgorithm::Crc32) => {
				self.crc32 = Some(Crc32::new());
			}
			Some(ChecksumAlgorithm::Crc32c) => {
				self.crc32c = Some(Crc32c::default());
			}
			Some(ChecksumAlgorithm::Sha1) => {
				self.sha1 = Some(Sha1::new());
			}
			Some(ChecksumAlgorithm::Sha256) => {
				self.sha256 = Some(Sha256::new());
			}
			None => (),
		}
		self
	}

	pub fn update(&mut self, bytes: &[u8]) {
		if let Some(crc32) = &mut self.crc32 {
			crc32.update(bytes);
		}
		if let Some(crc32c) = &mut self.crc32c {
			crc32c.write(bytes);
		}
		if let Some(md5) = &mut self.md5 {
			md5.update(bytes);
		}
		if let Some(sha1) = &mut self.sha1 {
			sha1.update(bytes);
		}
		if let Some(sha256) = &mut self.sha256 {
			sha256.update(bytes);
		}
	}

	pub fn finalize(self) -> Checksums {
		Checksums {
			crc32: self.crc32.map(|x| u32::to_be_bytes(x.finalize())),
			crc32c: self
				.crc32c
				.map(|x| u32::to_be_bytes(u32::try_from(x.finish()).unwrap())),
			md5: self.md5.map(|x| x.finalize()[..].try_into().unwrap()),
			sha1: self.sha1.map(|x| x.finalize()[..].try_into().unwrap()),
			sha256: self.sha256.map(|x| x.finalize()[..].try_into().unwrap()),
		}
	}
}

impl Checksums {
	pub fn verify(&self, expected: &ExpectedChecksums) -> Result<(), Error> {
		if let Some(expected_md5) = &expected.md5 {
			match self.md5 {
				Some(md5) if BASE64_STANDARD.encode(&md5) == expected_md5.trim_matches('"') => (),
				_ => {
					return Err(Error::InvalidDigest(
						"MD5 checksum verification failed (from content-md5)".into(),
					))
				}
			}
		}
		if let Some(expected_sha256) = &expected.sha256 {
			match self.sha256 {
				Some(sha256) if &sha256[..] == expected_sha256.as_slice() => (),
				_ => {
					return Err(Error::InvalidDigest(
						"SHA256 checksum verification failed (from x-amz-content-sha256)".into(),
					))
				}
			}
		}
		if let Some(extra) = expected.extra {
			let algo = extra.algorithm();
			if self.extract(Some(algo)) != Some(extra) {
				return Err(Error::InvalidDigest(format!(
					"Failed to validate checksum for algorithm {:?}",
					algo
				)));
			}
		}
		Ok(())
	}

	pub fn extract(&self, algo: Option<ChecksumAlgorithm>) -> Option<ChecksumValue> {
		match algo {
			None => None,
			Some(ChecksumAlgorithm::Crc32) => Some(ChecksumValue::Crc32(self.crc32.unwrap())),
			Some(ChecksumAlgorithm::Crc32c) => Some(ChecksumValue::Crc32c(self.crc32c.unwrap())),
			Some(ChecksumAlgorithm::Sha1) => Some(ChecksumValue::Sha1(self.sha1.unwrap())),
			Some(ChecksumAlgorithm::Sha256) => Some(ChecksumValue::Sha256(self.sha256.unwrap())),
		}
	}
}
