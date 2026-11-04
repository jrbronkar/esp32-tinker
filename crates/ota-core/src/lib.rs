#![no_std]
//! Bounded unsigned development updater. Authentication is a later milestone.
use sha2::{Digest, Sha256};

pub const APP: &str = "device-demo";
pub const MARKER: &str = "rust-dev-ota-v1";
pub const SLOT_SIZE: u32 = 0x300000;
pub const CHUNK: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    State,
    Metadata,
    Bounds,
    Order,
    Storage,
    Hash,
    Image,
}

pub trait Reader {
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Error>;
}
pub trait Target: Reader {
    fn capacity(&self) -> u32;
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Error>;
    fn commit(&mut self) -> Result<(), Error>;
}

pub struct SliceImage<'a>(pub &'a [u8]);
impl Reader for SliceImage<'_> {
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Error> {
        let end = (offset as usize)
            .checked_add(bytes.len())
            .ok_or(Error::Bounds)?;
        let source = self.0.get(offset as usize..end).ok_or(Error::Bounds)?;
        bytes.copy_from_slice(source);
        Ok(())
    }
}

pub fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn cstr(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())]
}

/// Validate ESP32-S3 application structure, project/version/capability descriptor,
/// segment checksum, and mandatory ESP image SHA-256 footer. Never accepts ELF
/// or merged bootloader images. Does not replace bootloader load-address checks.
pub fn validate_image(reader: &mut impl Reader, length: u32, version: &str) -> Result<(), Error> {
    if length < 288 || length > SLOT_SIZE {
        return Err(Error::Bounds);
    }
    let mut head = [0u8; 176];
    reader.read(0, &mut head)?;
    if head[0] != 0xe9
        || !(1..=16).contains(&head[1])
        || head[12..14] != [9, 0]
        || head[23] != 1
        || head[32..36] != 0xabcd5432u32.to_le_bytes()
        || cstr(&head[80..112]) != APP.as_bytes()
        || cstr(&head[48..80]) != version.as_bytes()
        || cstr(&head[144..176]) != MARKER.as_bytes()
    {
        return Err(Error::Image);
    }
    let mut cursor = 24u32;
    let mut checksum = 0xefu8;
    let mut buffer = [0u8; 512];
    for index in 0..head[1] {
        let mut segment = [0u8; 8];
        reader.read(cursor, &mut segment)?;
        let size = u32::from_le_bytes(segment[4..].try_into().unwrap());
        cursor = cursor.checked_add(8).ok_or(Error::Bounds)?;
        if index == 0 && size < 256 {
            return Err(Error::Image);
        }
        let end = cursor.checked_add(size).ok_or(Error::Bounds)?;
        if end > length {
            return Err(Error::Bounds);
        }
        while cursor < end {
            let n = (end - cursor).min(buffer.len() as u32) as usize;
            reader.read(cursor, &mut buffer[..n])?;
            for byte in &buffer[..n] {
                checksum ^= byte;
            }
            cursor += n as u32;
        }
    }
    let checksum_offset = cursor.checked_add(16).ok_or(Error::Bounds)? & !15;
    if checksum_offset.checked_add(32) != Some(length) {
        return Err(Error::Image);
    }
    let mut stored_checksum = [0u8; 1];
    reader.read(checksum_offset - 1, &mut stored_checksum)?;
    if stored_checksum[0] != checksum {
        return Err(Error::Image);
    }
    let mut digest = Sha256::new();
    cursor = 0;
    while cursor < checksum_offset {
        let n = (checksum_offset - cursor).min(buffer.len() as u32) as usize;
        reader.read(cursor, &mut buffer[..n])?;
        digest.update(&buffer[..n]);
        cursor += n as u32;
    }
    let mut footer = [0u8; 32];
    reader.read(checksum_offset, &mut footer)?;
    if <[u8; 32]>::from(digest.finalize()) != footer {
        return Err(Error::Hash);
    }
    Ok(())
}

pub struct Update {
    size: u32,
    expected: [u8; 32],
    version: [u8; 32],
    version_len: usize,
    received: u32,
    verified: bool,
}
impl Update {
    pub fn begin(
        target: &impl Target,
        application: &str,
        chip: &str,
        version: &str,
        size: u32,
        digest: &[u8],
    ) -> Result<Self, Error> {
        if application != APP
            || chip != "esp32s3"
            || version.is_empty()
            || version.len() > 31
            || version.as_bytes().contains(&0)
            || digest.len() != 32
        {
            return Err(Error::Metadata);
        }
        if size < 288 || size > target.capacity() || size > SLOT_SIZE {
            return Err(Error::Bounds);
        }
        let mut value = Self {
            size,
            expected: digest.try_into().unwrap(),
            version: [0; 32],
            version_len: version.len(),
            received: 0,
            verified: false,
        };
        value.version[..version.len()].copy_from_slice(version.as_bytes());
        Ok(value)
    }
    pub fn received(&self) -> u32 {
        self.received
    }
    pub fn chunk(
        &mut self,
        target: &mut impl Target,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), Error> {
        if self.verified {
            return Err(Error::State);
        }
        if offset != self.received {
            return Err(Error::Order);
        }
        let remaining = self.size - self.received;
        if bytes.len() != (remaining as usize).min(CHUNK) || bytes.is_empty() {
            return Err(Error::Bounds);
        }
        // The complete first header/descriptor arrives before the first flash write.
        if offset == 0 {
            if bytes.len() < 176
                || bytes[0] != 0xe9
                || bytes[12..14] != [9, 0]
                || bytes[32..36] != 0xabcd5432u32.to_le_bytes()
                || cstr(&bytes[80..112]) != APP.as_bytes()
                || cstr(&bytes[144..176]) != MARKER.as_bytes()
                || cstr(&bytes[48..80]) != &self.version[..self.version_len]
            {
                return Err(Error::Image);
            }
        }
        target.write(offset, bytes)?;
        self.received += bytes.len() as u32;
        Ok(())
    }
    pub fn finish(&mut self, target: &mut impl Target) -> Result<(), Error> {
        if self.received != self.size || self.verified {
            return Err(Error::State);
        }
        let mut digest = Sha256::new();
        let mut offset = 0;
        let mut buffer = [0u8; 512];
        while offset < self.size {
            let n = (self.size - offset).min(512) as usize;
            target.read(offset, &mut buffer[..n])?;
            digest.update(&buffer[..n]);
            offset += n as u32;
        }
        if <[u8; 32]>::from(digest.finalize()) != self.expected {
            return Err(Error::Hash);
        }
        let version =
            core::str::from_utf8(&self.version[..self.version_len]).map_err(|_| Error::Metadata)?;
        validate_image(target, self.size, version)?;
        self.verified = true;
        Ok(())
    }
    pub fn commit(&mut self, target: &mut impl Target) -> Result<(), Error> {
        if !self.verified {
            return Err(Error::State);
        }
        target.commit()?;
        self.verified = false;
        Ok(())
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests {
    use super::*;
    use std::{vec, vec::Vec};
    fn image(size: usize) -> Vec<u8> {
        let end = 32 + size;
        let footer = (end + 16) & !15;
        let mut bytes = vec![0u8; footer + 32];
        bytes[0] = 0xe9;
        bytes[1] = 1;
        bytes[12] = 9;
        bytes[23] = 1;
        bytes[28..32].copy_from_slice(&(size as u32).to_le_bytes());
        bytes[32..36].copy_from_slice(&0xabcd5432u32.to_le_bytes());
        bytes[48..50].copy_from_slice(b"v1");
        bytes[80..80 + APP.len()].copy_from_slice(APP.as_bytes());
        bytes[144..144 + MARKER.len()].copy_from_slice(MARKER.as_bytes());
        bytes[footer - 1] = bytes[32..end].iter().fold(0xef, |a, b| a ^ b);
        let digest = hash(&bytes[..footer]);
        bytes[footer..].copy_from_slice(&digest);
        bytes
    }
    struct Memory {
        bytes: Vec<u8>,
        writes: usize,
        commits: usize,
    }
    impl Memory {
        fn new() -> Self {
            Self {
                bytes: vec![0xff; 16384],
                writes: 0,
                commits: 0,
            }
        }
    }
    impl Reader for Memory {
        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Error> {
            SliceImage(&self.bytes).read(offset, bytes)
        }
    }
    impl Target for Memory {
        fn capacity(&self) -> u32 {
            self.bytes.len() as u32
        }
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Error> {
            self.bytes[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
            self.writes += 1;
            Ok(())
        }
        fn commit(&mut self) -> Result<(), Error> {
            self.commits += 1;
            Ok(())
        }
    }
    #[test]
    fn ordered_upload_verified_from_flash_before_commit() {
        let image = image(8192);
        let mut target = Memory::new();
        let mut update = Update::begin(
            &target,
            APP,
            "esp32s3",
            "v1",
            image.len() as u32,
            &hash(&image),
        )
        .unwrap();
        assert_eq!(update.commit(&mut target), Err(Error::State));
        assert_eq!(update.finish(&mut target), Err(Error::State));
        for (i, bytes) in image.chunks(CHUNK).enumerate() {
            update
                .chunk(&mut target, (i * CHUNK) as u32, bytes)
                .unwrap();
        }
        update.finish(&mut target).unwrap();
        assert_eq!(target.commits, 0);
        assert_eq!(
            update.chunk(&mut target, 0, &image[..CHUNK]),
            Err(Error::State)
        );
        update.commit(&mut target).unwrap();
        assert_eq!(target.commits, 1);
        assert_eq!(update.commit(&mut target), Err(Error::State));
    }
    #[test]
    fn reject_metadata_bounds_and_out_of_order_without_writing() {
        let image = image(256);
        let mut target = Memory::new();
        assert!(Update::begin(
            &target,
            "imu-demo",
            "esp32s3",
            "v1",
            image.len() as u32,
            &hash(&image)
        )
        .is_err());
        assert!(Update::begin(
            &target,
            APP,
            "esp32",
            "v1",
            image.len() as u32,
            &hash(&image)
        )
        .is_err());
        assert!(Update::begin(&target, APP, "esp32s3", "v1", u32::MAX, &hash(&image)).is_err());
        let mut update = Update::begin(
            &target,
            APP,
            "esp32s3",
            "v1",
            image.len() as u32,
            &hash(&image),
        )
        .unwrap();
        assert_eq!(update.chunk(&mut target, 1, &image), Err(Error::Order));
        assert_eq!(
            update.chunk(&mut target, 0, &image[..100]),
            Err(Error::Bounds)
        );
        let mut bad = image.clone();
        bad[144] = 0;
        assert_eq!(update.chunk(&mut target, 0, &bad), Err(Error::Image));
        assert_eq!(target.writes, 0);
    }
    #[test]
    fn corruption_in_flash_prevents_activation() {
        let image = image(256);
        let mut target = Memory::new();
        let mut update = Update::begin(
            &target,
            APP,
            "esp32s3",
            "v1",
            image.len() as u32,
            &hash(&image),
        )
        .unwrap();
        update.chunk(&mut target, 0, &image).unwrap();
        target.bytes[200] ^= 1;
        assert_eq!(update.finish(&mut target), Err(Error::Hash));
        assert_eq!(update.commit(&mut target), Err(Error::State));
        assert_eq!(target.commits, 0);
    }
    #[test]
    fn segment_checksum_footer_and_version_are_validated() {
        let image = image(256);
        validate_image(&mut SliceImage(&image), image.len() as u32, "v1").unwrap();
        assert!(validate_image(&mut SliceImage(&image), image.len() as u32, "v2").is_err());
        let mut bad = image.clone();
        bad[303] ^= 1;
        assert_eq!(
            validate_image(&mut SliceImage(&bad), bad.len() as u32, "v1"),
            Err(Error::Image)
        );
        let mut bad = image.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(
            validate_image(&mut SliceImage(&bad), bad.len() as u32, "v1"),
            Err(Error::Hash)
        );
        assert!(validate_image(&mut SliceImage(&image[..300]), 300, "v1").is_err());
    }
    #[test]
    fn storage_failures_never_report_success() {
        struct Faulty {
            memory: Memory,
            fail: &'static str,
        }
        impl Reader for Faulty {
            fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Error> {
                if self.fail == "read" {
                    Err(Error::Storage)
                } else {
                    self.memory.read(offset, bytes)
                }
            }
        }
        impl Target for Faulty {
            fn capacity(&self) -> u32 {
                self.memory.capacity()
            }
            fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Error> {
                if self.fail == "write" {
                    Err(Error::Storage)
                } else {
                    self.memory.write(offset, bytes)
                }
            }
            fn commit(&mut self) -> Result<(), Error> {
                if self.fail == "commit" {
                    Err(Error::Storage)
                } else {
                    self.memory.commit()
                }
            }
        }
        let image = image(256);
        for fail in ["write", "read", "commit"] {
            let mut target = Faulty {
                memory: Memory::new(),
                fail,
            };
            let mut update = Update::begin(
                &target,
                APP,
                "esp32s3",
                "v1",
                image.len() as u32,
                &hash(&image),
            )
            .unwrap();
            if fail == "write" {
                assert_eq!(update.chunk(&mut target, 0, &image), Err(Error::Storage));
                assert_eq!(update.received(), 0);
            } else {
                update.chunk(&mut target, 0, &image).unwrap();
                if fail == "read" {
                    assert_eq!(update.finish(&mut target), Err(Error::Storage));
                    assert_eq!(update.commit(&mut target), Err(Error::State));
                } else {
                    update.finish(&mut target).unwrap();
                    assert_eq!(update.commit(&mut target), Err(Error::Storage));
                }
            }
            assert_eq!(target.memory.commits, 0);
        }
    }
}
