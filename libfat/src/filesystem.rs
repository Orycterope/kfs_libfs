//! FAT Filesystem.

use arrayvec::ArrayString;
use byteorder::{ByteOrder, LittleEndian};

use super::attribute::Attributes;
use super::block_iter::BlockIndexClusterIter;
use super::directory::{dir_entry::DirectoryEntry, Directory};
use super::FatVolumeBootRecord;

use super::cluster::Cluster;
use super::table;
use super::table::FatValue;
use super::utils;
use super::FatFsType;
use libfs::block::{Block, BlockCount, BlockDevice, BlockIndex};
use libfs::FileSystemError;
use libfs::FileSystemResult;

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

/// Reprsent the FS Info structure of FAT32.
struct FatFileSystemInfo {
    // TODO: select Ordering wisely on operations.
    /// The last allocated cluster on the filesystem.
    last_cluster: AtomicU32,

    /// The free cluster count on the filesystem.
    free_cluster: AtomicU32,
}

impl FatFileSystemInfo {
    /// Import FS Info from a FAT32 filesystem.
    fn from_fs<T>(fs: &FatFileSystem<T>) -> FileSystemResult<Self>
    where
        T: BlockDevice,
    {
        let mut blocks = [Block::new()];

        let mut last_cluster = 0xFFFF_FFFF;
        let mut free_cluster = 0xFFFF_FFFF;

        fs.block_device
            .read(
                &mut blocks,
                fs.partition_start,
                BlockIndex(u32::from(fs.boot_record.fs_info_block())),
            )
            .or(Err(FileSystemError::ReadFailed))?;

        // valid signature?
        if &blocks[0][0..4] == b"RRaA"
            && &blocks[0][0x1e4..0x1e8] == b"rrAa"
            && LittleEndian::read_u16(&blocks[0][0x1fe..0x200]) == 0xAA55
        {
            // check cluster sanity
            let fs_last_cluster = LittleEndian::read_u32(&blocks[0][0x1ec..0x1f0]);
            if fs_last_cluster >= 2 && fs_last_cluster < fs.boot_record.cluster_count {
                last_cluster = fs_last_cluster;
            }

            // check sanity
            let fs_free_cluster = LittleEndian::read_u32(&blocks[0][0x1e8..0x1ec]);
            if fs_free_cluster <= fs.boot_record.cluster_count {
                free_cluster = fs_free_cluster;
            }
        }

        Ok(FatFileSystemInfo {
            last_cluster: AtomicU32::new(last_cluster),
            free_cluster: AtomicU32::new(free_cluster),
        })
    }

    /// Flush the FS Info to the disk on FAT32 filesystems.
    fn flush<T>(&self, fs: &FatFileSystem<T>) -> FileSystemResult<()>
    where
        T: BlockDevice,
    {
        if fs.boot_record.fat_type != FatFsType::Fat32 {
            return Ok(());
        }

        let mut blocks = [Block::new()];

        LittleEndian::write_u32(&mut blocks[0][0..4], 0x4161_5252);
        LittleEndian::write_u32(&mut blocks[0][0x1e4..0x1e8], 0x6141_7272);
        LittleEndian::write_u16(&mut blocks[0][0x1fe..0x200], 0xAA55);

        LittleEndian::write_u32(
            &mut blocks[0][0x1ec..0x1f0],
            self.last_cluster.load(Ordering::SeqCst),
        );
        LittleEndian::write_u32(
            &mut blocks[0][0x1e8..0x1ec],
            self.free_cluster.load(Ordering::SeqCst),
        );

        fs.block_device
            .write(
                &blocks,
                fs.partition_start,
                BlockIndex(u32::from(fs.boot_record.fs_info_block())),
            )
            .or(Err(FileSystemError::ReadFailed))?;

        Ok(())
    }
}

/// Represent a FAT filesystem.
#[allow(dead_code)]
pub struct FatFileSystem<T> {
    /// The block device of the filesystem.
    pub(crate) block_device: T,

    /// The block index of the start of the partition of this filesystem.
    pub(crate) partition_start: BlockIndex,

    /// Block index of the first block availaible for data.
    pub(crate) first_data_offset: BlockIndex,

    // TODO: check we don't go out of the partition
    /// The count of blocks that this partition contains.
    pub(crate) partition_block_count: BlockCount,

    /// The volume information of the filesystem.
    pub(crate) boot_record: FatVolumeBootRecord,

    /// The extra infos of the filesystem.
    fat_info: FatFileSystemInfo,
}

impl<T> FatFileSystem<T>
where
    T: BlockDevice,
{
    /// Create a new instance of FatFileSystem
    /// TODO: ``init`` needs to be called after this
    pub(crate) fn new(
        block_device: T,
        partition_start: BlockIndex,
        first_data_offset: BlockIndex,
        partition_block_count: BlockCount,
        boot_record: FatVolumeBootRecord,
    ) -> FatFileSystem<T> {
        FatFileSystem {
            block_device,
            partition_start,
            first_data_offset,
            partition_block_count,
            boot_record,
            fat_info: FatFileSystemInfo {
                last_cluster: AtomicU32::new(0xFFFF_FFFF),
                free_cluster: AtomicU32::new(0xFFFF_FFFF),
            },
        }
    }

    /// Initialize the filesystem.
    pub(crate) fn init(&mut self) -> FileSystemResult<()> {
        // read FAT infos
        if self.boot_record.fat_type == FatFsType::Fat32 {
            self.fat_info = FatFileSystemInfo::from_fs(self)?;
        }

        if self.fat_info.free_cluster.load(Ordering::SeqCst) == 0xFFFF_FFFF {
            self.fat_info
                .free_cluster
                .store(table::get_free_cluster_count(self)?, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Get the root directory of the filesystem.
    pub fn get_root_directory(&self) -> Directory<'_, T> {
        let dir_info = DirectoryEntry {
            start_cluster: self.boot_record.root_dir_childs_cluster(),
            raw_info: None,
            file_size: 0,
            creation_timestamp: 0,
            last_access_timestamp: 0,
            last_modification_timestamp: 0,
            file_name: ArrayString::<[_; DirectoryEntry::MAX_FILE_NAME_LEN_UNICODE]>::new(),
            attribute: Attributes::new(Attributes::DIRECTORY),
        };

        Directory::from_entry(self, dir_info)
    }

    /// Create a new directory at the given path.
    pub fn mkdir(&self, path: &str) -> FileSystemResult<()> {
        let (parent_name, file_name) = utils::get_parent(path);
        let mut parent_dir = if parent_name == "" {
            self.get_root_directory()
        } else {
            self.get_root_directory().open_dir(parent_name)?
        };

        // precheck that it doesn't exist already
        if parent_dir.clone().find_entry(file_name).is_ok() {
            return Err(FileSystemError::FileExists);
        }

        parent_dir.mkdir(file_name)
    }

    /// Create a new file at the given path.
    pub fn touch(&self, path: &str) -> FileSystemResult<()> {
        let (parent_name, file_name) = utils::get_parent(path);
        let mut parent_dir = if parent_name == "" {
            self.get_root_directory()
        } else {
            self.get_root_directory().open_dir(parent_name)?
        };

        // precheck that it doesn't exist already
        if parent_dir.clone().find_entry(file_name).is_ok() {
            return Err(FileSystemError::FileExists);
        }

        parent_dir.touch(file_name)
    }

    /// Delete a directory or a file at the given path.
    pub fn unlink(&self, path: &str, is_dir: bool) -> FileSystemResult<()> {
        let (parent_name, file_name) = utils::get_parent(path);
        let parent_dir = if parent_name == "" {
            self.get_root_directory()
        } else {
            self.get_root_directory().open_dir(parent_name)?
        };

        parent_dir.unlink(file_name, is_dir)
    }

    /// Rename a directory or a file at the given path to a new path.
    pub fn rename(&self, old_path: &str, new_path: &str, is_dir: bool) -> FileSystemResult<()> {
        let (parent_name, file_name) = utils::get_parent(old_path);
        let parent_old_dir = if parent_name == "" {
            self.get_root_directory()
        } else {
            self.get_root_directory().open_dir(parent_name)?
        };

        let old_entry = parent_old_dir.find_entry(file_name)?;

        if old_entry.attribute.is_directory() != is_dir {
            if is_dir {
                return Err(FileSystemError::NotADirectory);
            } else {
                return Err(FileSystemError::NotAFile);
            }
        }

        let (parent_name, file_name) = utils::get_parent(new_path);
        let parent_new_dir = if parent_name == "" {
            self.get_root_directory()
        } else {
            self.get_root_directory().open_dir(parent_name)?
        };

        if parent_new_dir.clone().find_entry(file_name).is_ok() {
            return Err(FileSystemError::FileExists);
        }

        parent_new_dir.rename(old_entry, file_name, is_dir)
    }

    /// Clean cluster chain data.
    /// Used when creating a new directory.
    pub(crate) fn clean_cluster_data(&self, cluster: Cluster) -> FileSystemResult<()> {
        let blocks = [Block::new()];
        let mut block_index = 0;

        for cluster in BlockIndexClusterIter::new(self, cluster, None) {
            block_index = (block_index + 1) % u32::from(self.boot_record.blocks_per_cluster());
            self.block_device
                .write(
                    &blocks,
                    self.partition_start,
                    BlockIndex(cluster.to_data_block_index(self).0 + block_index),
                )
                .or(Err(FileSystemError::WriteFailed))?;
        }

        Ok(())
    }

    /// Allocate a cluster and if specified add it to a cluster chain.
    pub(crate) fn alloc_cluster(
        &self,
        last_cluster_allocated_opt: Option<Cluster>,
    ) -> FileSystemResult<Cluster> {
        let mut start_cluster = Cluster(self.fat_info.last_cluster.load(Ordering::SeqCst));
        let mut resize_existing_cluster = false;

        if last_cluster_allocated_opt.is_none() {
            start_cluster = Cluster(self.fat_info.last_cluster.load(Ordering::SeqCst));
            if start_cluster.0 == 0 || start_cluster.0 >= self.boot_record.cluster_count {
                start_cluster = Cluster(1);
            }
        } else if let Some(last_cluster_allocated) = last_cluster_allocated_opt {
            start_cluster = last_cluster_allocated;

            let cluster_val = FatValue::get(self, start_cluster)?;
            if let FatValue::Data(valid_cluster) = cluster_val {
                if valid_cluster < self.boot_record.cluster_count {
                    return Ok(Cluster(valid_cluster));
                }
            }

            resize_existing_cluster = true;
        }

        if self.fat_info.free_cluster.load(Ordering::SeqCst) == 0 {
            return Err(FileSystemError::NoSpaceLeft);
        }

        let mut number_cluster = 0;

        // Resize of exisiting cluster?
        if resize_existing_cluster {
            // test next chunk
            number_cluster = start_cluster.0 + 1;
            if number_cluster >= self.boot_record.cluster_count {
                number_cluster = 2;
            }

            let value = FatValue::get(self, Cluster(number_cluster))?;

            if value != FatValue::Free {
                let new_start = Cluster(self.fat_info.last_cluster.load(Ordering::SeqCst));
                if new_start.0 >= 2 && new_start.0 < self.boot_record.cluster_count {
                    start_cluster = new_start;
                }

                number_cluster = 0;
            }
        }

        if number_cluster == 0 {
            number_cluster = start_cluster.0;
            loop {
                number_cluster += 1;
                if number_cluster >= self.boot_record.cluster_count {
                    number_cluster = 2;
                    if number_cluster > start_cluster.0 {
                        return Err(FileSystemError::NoSpaceLeft);
                    }
                }

                let value = FatValue::get(self, Cluster(number_cluster))?;

                if value == FatValue::Free {
                    break;
                }

                if number_cluster == start_cluster.0 {
                    return Err(FileSystemError::NoSpaceLeft);
                }
            }
        }

        let allocated_cluster = Cluster(number_cluster);
        debug_assert!(FatValue::get(self, allocated_cluster)? == FatValue::Free);
        FatValue::put(self, allocated_cluster, FatValue::EndOfChain)?;

        // Link existing cluster with the new one availaible
        if let Some(last_cluster_allocated) = last_cluster_allocated_opt {
            debug_assert!(
                FatValue::get(self, last_cluster_allocated)? == FatValue::Free
                    || FatValue::get(self, last_cluster_allocated)? == FatValue::EndOfChain
            );
            FatValue::put(
                self,
                last_cluster_allocated,
                FatValue::Data(allocated_cluster.0),
            )?;
        }

        self.fat_info
            .last_cluster
            .store(allocated_cluster.0, Ordering::SeqCst);
        self.fat_info.free_cluster.fetch_sub(1, Ordering::SeqCst);
        self.fat_info.flush(self)?;

        Ok(allocated_cluster)
    }

    /// Free a cluster and if specified remove of a cluster chain.
    pub(crate) fn free_cluster(
        &self,
        to_remove: Cluster,
        previous_cluster: Option<Cluster>,
    ) -> FileSystemResult<()> {
        if let Some(previous_cluster) = previous_cluster {
            FatValue::put(self, previous_cluster, FatValue::EndOfChain)?;
        }

        let mut current_cluster = to_remove;

        loop {
            let value = FatValue::get(self, current_cluster)?;

            if value == FatValue::Free {
                break;
            }

            FatValue::put(self, current_cluster, FatValue::Free)?;

            // Invalidate last cluster if equals to the current cluster
            self.fat_info.last_cluster.compare_and_swap(
                0xFFFF_FFFF,
                current_cluster.0,
                Ordering::SeqCst,
            );

            self.fat_info.free_cluster.fetch_add(1, Ordering::SeqCst);

            match value {
                FatValue::Data(data) => {
                    current_cluster = Cluster(data);
                }
                _ => break,
            }
        }
        self.fat_info.flush(self)?;
        Ok(())
    }
}
