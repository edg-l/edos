use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct Disk {
    file: File,
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
        if file.read(&mut magic)? == 4 && magic == *b"QFI\xfb" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is a qcow2 image; fsck reads raw images and block devices only. \
                     Convert it first: qemu-img convert -O raw {} <raw image>",
                    path.display(),
                    path.display()
                ),
            ));
        }
        Ok(Disk {
            file,
            partition_offset,
            block_size,
        })
    }

    fn block_byte_offset(&self, block_num: u64) -> u64 {
        self.partition_offset + block_num * self.block_size as u64
    }

    pub fn read_block(&mut self, block_num: u64) -> io::Result<Vec<u8>> {
        let offset = self.block_byte_offset(block_num);
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; self.block_size as usize];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn write_block(&mut self, block_num: u64, data: &[u8]) -> io::Result<()> {
        let offset = self.block_byte_offset(block_num);
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        let pad = self.block_size as usize - data.len();
        if pad > 0 {
            let zeros = vec![0u8; pad];
            self.file.write_all(&zeros)?;
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
        self.file.seek(SeekFrom::Start(offset))?;
        let mut val = std::mem::MaybeUninit::<T>::zeroed();
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(val.as_mut_ptr() as *mut u8, std::mem::size_of::<T>())
        };
        self.file.read_exact(bytes)?;
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
        self.file.seek(SeekFrom::Start(offset))?;
        let bytes = unsafe {
            std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>())
        };
        self.file.write_all(bytes)?;
        Ok(())
    }

    pub fn fsync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Return the total byte size of the underlying file/device.
    pub fn file_size(&mut self) -> io::Result<u64> {
        let pos = self.file.seek(SeekFrom::End(0))?;
        Ok(pos)
    }
}
