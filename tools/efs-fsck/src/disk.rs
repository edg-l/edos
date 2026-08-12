use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::qcow2::{self, Qcow2};

/// What the checker reads through: a raw image or block device, or a qcow2
/// image decoded on the fly. qcow2 is read-only — writing one means allocating
/// clusters and maintaining refcounts, which `--repair` has no business doing.
enum Image {
    Raw(std::fs::File),
    Qcow2(Box<Qcow2>),
}

pub struct Disk {
    image: Image,
    pub partition_offset: u64,
    pub block_size: u32,
}

#[allow(dead_code)]
impl Disk {
    pub fn open(
        path: &Path,
        repair: bool,
        partition_offset: u64,
        block_size: u32,
    ) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(repair).open(path)?;
        let mut magic = [0u8; 4];
        let image = if file.read(&mut magic)? == 4 && magic == qcow2::MAGIC {
            if repair {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is a qcow2 image; fsck can check one but not repair it. \
                         Convert it first: qemu-img convert -O raw {} <raw image>",
                        path.display(),
                        path.display()
                    ),
                ));
            }
            Image::Qcow2(Box::new(Qcow2::open(file)?))
        } else {
            Image::Raw(file)
        };
        Ok(Disk {
            image,
            partition_offset,
            block_size,
        })
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        match &mut self.image {
            Image::Raw(file) => {
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(buf)
            }
            Image::Qcow2(img) => img.read_at(offset, buf),
        }
    }

    /// The writable half of the image, which only a raw one has.
    fn writer(&mut self) -> io::Result<&mut std::fs::File> {
        match &mut self.image {
            Image::Raw(file) => Ok(file),
            Image::Qcow2(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot write to a qcow2 image",
            )),
        }
    }

    fn block_byte_offset(&self, block_num: u64) -> u64 {
        self.partition_offset + block_num * self.block_size as u64
    }

    pub fn read_block(&mut self, block_num: u64) -> io::Result<Vec<u8>> {
        let offset = self.block_byte_offset(block_num);
        let mut buf = vec![0u8; self.block_size as usize];
        self.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    pub fn write_block(&mut self, block_num: u64, data: &[u8]) -> io::Result<()> {
        let offset = self.block_byte_offset(block_num);
        self.write_padded_block_at(offset, data)
    }

    /// Write a block addressed from the start of the *device* rather than the
    /// partition.
    ///
    /// Journal descriptor entries carry device-absolute block numbers (see
    /// [`efs_common::DescriptorEntry::fs_block`]): the kernel derives one from
    /// `block_to_lba(block) / sectors_per_block`, which already includes the
    /// partition's starting LBA. Replay must therefore not add
    /// `partition_offset` a second time, or every home block lands
    /// `partition_offset` bytes too high. Every other block number the checker
    /// handles is partition-relative and belongs in [`Self::write_block`].
    pub fn write_device_block(&mut self, device_block: u64, data: &[u8]) -> io::Result<()> {
        let offset = device_block * self.block_size as u64;
        self.write_padded_block_at(offset, data)
    }

    fn write_padded_block_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let pad = self.block_size as usize - data.len();
        let file = self.writer()?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        if pad > 0 {
            let zeros = vec![0u8; pad];
            file.write_all(&zeros)?;
        }
        Ok(())
    }

    /// Read a struct `T` at `offset_in_block` bytes within block `block_num`.
    ///
    /// # Safety
    /// T must be `#[repr(C)]` with no uninitialized padding bytes.
    pub fn read_struct_at<T: Copy>(
        &mut self,
        block_num: u64,
        offset_in_block: usize,
    ) -> io::Result<T> {
        let offset = self.block_byte_offset(block_num) + offset_in_block as u64;
        let mut val = std::mem::MaybeUninit::<T>::zeroed();
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(val.as_mut_ptr() as *mut u8, std::mem::size_of::<T>())
        };
        self.read_at(offset, bytes)?;
        Ok(unsafe { val.assume_init() })
    }

    /// Write a struct `T` at `offset_in_block` bytes within block `block_num`.
    ///
    /// # Safety
    /// T must be `#[repr(C)]` with no uninitialized padding bytes.
    pub fn write_struct_at<T>(
        &mut self,
        block_num: u64,
        offset_in_block: usize,
        val: &T,
    ) -> io::Result<()> {
        let offset = self.block_byte_offset(block_num) + offset_in_block as u64;
        let bytes = unsafe {
            std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>())
        };
        let file = self.writer()?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
        Ok(())
    }

    pub fn fsync(&mut self) -> io::Result<()> {
        match &mut self.image {
            Image::Raw(file) => file.sync_all(),
            Image::Qcow2(_) => Ok(()),
        }
    }

    /// Total byte size of the image as the filesystem sees it: the file for a
    /// raw image, the virtual size for a qcow2 one.
    pub fn file_size(&mut self) -> io::Result<u64> {
        match &mut self.image {
            Image::Raw(file) => file.seek(SeekFrom::End(0)),
            Image::Qcow2(img) => Ok(img.virtual_size()),
        }
    }
}
