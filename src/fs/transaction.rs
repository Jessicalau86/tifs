use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::time::SystemTime;

use bytes::Bytes;
use bytestring::ByteString;
use fuser::{FileAttr, FileType};
use libc::F_UNLCK;
use tikv_client::{Transaction, TransactionClient};
use tracing::{debug, trace};

use super::block::empty_block;
use super::dir::Directory;
use super::error::{FsError, Result};
use super::index::{IndexKey, IndexValue};
use super::inode::{Inode, LockState};
use super::key::{ScopedKey, ROOT_INODE};
use super::meta::Meta;
use super::mode::{as_file_kind, as_file_perm, make_mode};
use super::reply::DirItem;
use super::tikv_fs::TiFs;

pub struct Txn(Transaction);

impl Txn {
    pub async fn begin_optimistic(client: &TransactionClient) -> Result<Self> {
        Ok(Txn(client.begin_optimistic().await?))
    }

    pub async fn make_inode(
        &mut self,
        parent: u64,
        name: ByteString,
        mode: u32,
        gid: u32,
        uid: u32,
    ) -> Result<Inode> {
        let mut meta = self.read_meta().await?.unwrap_or_default();
        let ino = meta.inode_next;
        meta.inode_next += 1;

        debug!("get ino({})", ino);
        self.save_meta(&meta).await?;

        let file_type = as_file_kind(mode);
        if parent >= ROOT_INODE {
            if self.get_index(parent, name.clone()).await?.is_some() {
                return Err(FsError::FileExist {
                    file: name.to_string(),
                });
            }
            self.set_index(parent, name.clone(), ino).await?;

            let mut dir = self.read_dir(parent).await?;
            debug!("read dir({:?})", &dir);

            dir.push(DirItem {
                ino,
                name: name.to_string(),
                typ: file_type,
            });

            self.save_dir(parent, &dir).await?;
            // TODO: update attributes of directory
        }

        let inode = Inode {
            file_attr: FileAttr {
                ino,
                size: 0,
                blocks: 0,
                atime: SystemTime::now(),
                mtime: SystemTime::now(),
                ctime: SystemTime::now(),
                crtime: SystemTime::now(),
                kind: file_type,
                perm: as_file_perm(mode),
                nlink: 1,
                uid,
                gid,
                rdev: 0,
                blksize: TiFs::BLOCK_SIZE as u32,
                padding: 0,
                flags: 0,
            },
            lock_state: LockState::new(HashSet::new(), F_UNLCK),
            inline_data: None,
        };

        debug!("made inode ({:?})", &inode);

        self.save_inode(&inode).await?;
        Ok(inode.into())
    }

    pub async fn get_index(&self, parent: u64, name: ByteString) -> Result<Option<u64>> {
        let key = IndexKey::new(parent, name.clone());
        self.get(key)
            .await
            .map_err(FsError::from)
            .and_then(|value| {
                value
                    .map(|data| Ok(IndexValue::deserialize(&data)?.ino))
                    .transpose()
            })
    }

    pub async fn set_index(&mut self, parent: u64, name: ByteString, ino: u64) -> Result<()> {
        let key = IndexKey::new(parent, name);
        let value = IndexValue::new(ino).serialize()?;
        Ok(self.put(key, value).await?)
    }

    pub async fn remove_index(&mut self, parent: u64, name: ByteString) -> Result<()> {
        let key = IndexKey::new(parent, name);
        Ok(self.delete(key).await?)
    }

    pub async fn read_inode(&self, ino: u64) -> Result<Inode> {
        let value = self
            .get(ScopedKey::inode(ino))
            .await?
            .ok_or_else(|| FsError::InodeNotFound { inode: ino })?;
        Ok(Inode::deserialize(&value)?)
    }

    pub async fn save_inode(&mut self, inode: &Inode) -> Result<()> {
        let key = ScopedKey::inode(inode.file_attr.ino).scoped();

        if inode.file_attr.nlink == 0 {
            self.delete(key).await?;
        } else {
            self.put(key, inode.serialize()?).await?;
            debug!("save inode: {:?}", inode);
        }
        Ok(())
    }

    pub async fn remove_inode(&mut self, ino: u64) -> Result<()> {
        self.delete(ScopedKey::inode(ino).scoped()).await?;
        Ok(())
    }

    pub async fn read_meta(&self) -> Result<Option<Meta>> {
        let opt_data = self.get(ScopedKey::meta().scoped()).await?;
        opt_data.map(|data| Meta::deserialize(&data)).transpose()
    }

    pub async fn save_meta(&mut self, meta: &Meta) -> Result<()> {
        self.put(ScopedKey::meta().scoped(), meta.serialize()?)
            .await?;
        Ok(())
    }

    async fn transfer_inline_data_to_block(&mut self, inode: &mut Inode) -> Result<()> {
        debug_assert!(inode.size <= TiFs::INLINE_DATA_THRESHOLD);
        let key = ScopedKey::new(inode.ino, 0).scoped();
        let mut data = inode.inline_data.clone().unwrap();
        data.resize(TiFs::BLOCK_SIZE as usize, 0);
        self.put(key, data).await?;
        inode.inline_data = None;
        Ok(())
    }

    async fn write_inline_data(
        &mut self,
        inode: &mut Inode,
        start: u64,
        data: &[u8],
    ) -> Result<usize> {
        debug_assert!(inode.size <= TiFs::INLINE_DATA_THRESHOLD);
        let size = data.len() as u64;
        debug_assert!(start + size <= TiFs::INLINE_DATA_THRESHOLD);

        let size = data.len();
        let start = start as usize;

        let mut inlined = inode.inline_data.take().unwrap_or_else(Vec::new);
        if start + size > inlined.len() {
            inlined.resize(start + size, 0);
        }
        inlined[start..start + size].copy_from_slice(data);

        inode.atime = SystemTime::now();
        inode.mtime = SystemTime::now();
        inode.ctime = SystemTime::now();
        inode.set_size(inlined.len() as u64);
        inode.inline_data = Some(inlined);
        self.save_inode(inode).await?;

        Ok(size)
    }

    async fn read_inline_data(
        &mut self,
        inode: &mut Inode,
        start: u64,
        size: u64,
    ) -> Result<Vec<u8>> {
        debug_assert!(inode.size <= TiFs::INLINE_DATA_THRESHOLD);

        let start = start as usize;
        let size = size as usize;

        let inlined = inode.inline_data.as_ref().unwrap();
        debug_assert!(inode.size as usize == inlined.len());
        let mut data: Vec<u8> = Vec::with_capacity(size);
        data.resize(size, 0);
        if inlined.len() > start {
            let to_copy = size.min(inlined.len() - start);
            data[..to_copy].copy_from_slice(&inlined[start..start + to_copy]);
        }

        inode.atime = SystemTime::now();
        self.save_inode(inode).await?;

        Ok(data)
    }

    pub async fn read_data(
        &mut self,
        ino: u64,
        start: u64,
        chunk_size: Option<u64>,
    ) -> Result<Vec<u8>> {
        let mut attr = self.read_inode(ino).await?;
        if start >= attr.size {
            return Ok(Vec::new());
        }

        let max_size = attr.size - start;
        let size = chunk_size.unwrap_or(max_size).min(max_size);

        if attr.inline_data.is_some() {
            return self.read_inline_data(&mut attr, start, size).await;
        }

        let target = start + size;
        let start_block = start / TiFs::BLOCK_SIZE;
        let end_block = (target + TiFs::BLOCK_SIZE - 1) / TiFs::BLOCK_SIZE;

        let pairs = self
            .scan(
                ScopedKey::block_range(ino, start_block..end_block),
                (end_block - start_block) as u32,
            )
            .await?;

        let mut data = pairs
            .enumerate()
            .flat_map(|(i, pair)| {
                let key: ScopedKey = pair.key().clone().into();
                let value = pair.into_value();
                (start_block as usize + i..key.key() as usize)
                    .map(|_| empty_block())
                    .chain(vec![value])
            })
            .enumerate()
            .fold(
                Vec::with_capacity(
                    ((end_block - start_block) * TiFs::BLOCK_SIZE - start % TiFs::BLOCK_SIZE)
                        as usize,
                ),
                |mut data, (i, value)| {
                    let mut slice = value.as_slice();
                    if i == 0 {
                        slice = &slice[(start % TiFs::BLOCK_SIZE) as usize..]
                    }

                    data.extend_from_slice(slice);
                    data
                },
            );

        data.resize(size as usize, 0);
        attr.atime = SystemTime::now();
        self.save_inode(&attr).await?;
        Ok(data)
    }

    pub async fn clear_data(&mut self, ino: u64) -> Result<u64> {
        let mut attr = self.read_inode(ino).await?;
        let end_block = (attr.size + TiFs::BLOCK_SIZE - 1) / TiFs::BLOCK_SIZE;

        for block in 0..end_block {
            self.delete(ScopedKey::new(ino, block).scoped()).await?;
        }

        let clear_size = attr.size;
        attr.size = 0;
        attr.atime = SystemTime::now();
        self.save_inode(&attr).await?;
        Ok(clear_size)
    }

    pub async fn write_data(&mut self, ino: u64, start: u64, data: Bytes) -> Result<usize> {
        debug!("write data at ({})[{}]", ino, start);
        let mut inode = self.read_inode(ino).await?;
        let size = data.len();
        let target = start + size as u64;

        if inode.inline_data.is_some() && target > TiFs::INLINE_DATA_THRESHOLD {
            self.transfer_inline_data_to_block(&mut inode).await?;
        }

        if (inode.inline_data.is_some() || inode.size == 0) && target <= TiFs::INLINE_DATA_THRESHOLD
        {
            return self.write_inline_data(&mut inode, start, &data).await;
        }

        let mut block_index = start / TiFs::BLOCK_SIZE;
        let start_key = ScopedKey::new(ino, block_index);
        let start_index = (start % TiFs::BLOCK_SIZE) as usize;

        let first_block_size = TiFs::BLOCK_SIZE as usize - start_index;

        let (first_block, mut rest) = data.split_at(first_block_size.min(data.len()));

        let mut start_value = self.get(start_key).await?.unwrap_or_else(empty_block);

        start_value[start_index..start_index + first_block.len()].copy_from_slice(first_block);

        self.put(start_key, start_value).await?;

        while rest.len() != 0 {
            block_index += 1;
            let key = ScopedKey::new(ino, block_index);
            let (curent_block, current_rest) =
                rest.split_at((TiFs::BLOCK_SIZE as usize).min(rest.len()));
            let mut value = curent_block.to_vec();
            if value.len() < TiFs::BLOCK_SIZE as usize {
                let mut last_value = self.get(key).await?.unwrap_or_else(empty_block);
                last_value[..value.len()].copy_from_slice(&value);
                value = last_value;
            }
            self.put(key, value).await?;
            rest = current_rest;
        }

        inode.atime = SystemTime::now();
        inode.mtime = SystemTime::now();
        inode.ctime = SystemTime::now();
        inode.set_size(inode.size.max(target));
        self.save_inode(&inode.into()).await?;
        trace!("write data: {}", String::from_utf8_lossy(&data));
        Ok(size)
    }

    pub async fn write_link(&mut self, inode: &mut Inode, data: Bytes) -> Result<usize> {
        debug_assert!(inode.file_attr.kind == FileType::Symlink);
        inode.inline_data = None;
        inode.set_size(0);
        self.write_inline_data(inode, 0, &data).await
    }

    pub async fn read_link(&mut self, ino: u64) -> Result<Vec<u8>> {
        let mut inode = self.read_inode(ino).await?;
        debug_assert!(inode.file_attr.kind == FileType::Symlink);
        let size = inode.size;
        self.read_inline_data(&mut inode, 0, size).await
    }

    pub async fn link(&mut self, ino: u64, newparent: u64, newname: ByteString) -> Result<Inode> {
        if let Some(old_ino) = self.get_index(newparent, newname.clone()).await? {
            let inode = self.read_inode(old_ino).await?;
            match inode.kind {
                FileType::Directory => self.rmdir(newparent, newname.clone()).await?,
                _ => self.unlink(newparent, newname.clone()).await?,
            }
        }
        self.set_index(newparent, newname.clone(), ino).await?;

        let mut inode = self.read_inode(ino).await?;
        let mut dir = self.read_dir(newparent).await?;

        dir.push(DirItem {
            ino,
            name: newname.to_string(),
            typ: inode.kind,
        });

        self.save_dir(newparent, &dir).await?;
        inode.nlink += 1;
        self.save_inode(&inode).await?;
        Ok(inode)
    }

    pub async fn unlink(&mut self, parent: u64, name: ByteString) -> Result<()> {
        match self.get_index(parent, name.clone()).await? {
            None => Err(FsError::FileNotFound {
                file: name.to_string(),
            }),
            Some(ino) => {
                self.remove_index(parent, name.clone()).await?;
                let parent_dir = self.read_dir(parent).await?;
                let new_parent_dir: Directory = parent_dir
                    .into_iter()
                    .filter(|item| item.name != &*name)
                    .collect();
                self.save_dir(parent, &new_parent_dir).await?;

                let mut inode = self.read_inode(ino).await?;
                inode.nlink -= 1;
                inode.ctime = SystemTime::now();
                self.save_inode(&inode).await?;
                Ok(())
            }
        }
    }

    pub async fn rmdir(&mut self, parent: u64, name: ByteString) -> Result<()> {
        match self.get_index(parent, name.clone()).await? {
            None => Err(FsError::FileNotFound {
                file: name.to_string(),
            }),
            Some(ino) => {
                let target_dir = self.read_dir(ino).await?;
                if target_dir.len() != 0 {
                    let name_str = name.to_string();
                    debug!("dir({}) not empty", &name_str);
                    return Err(FsError::DirNotEmpty { dir: name_str });
                }
                self.remove_index(parent, name.clone()).await?;
                self.remove_inode(ino).await?;

                let parent_dir = self.read_dir(parent).await?;
                let new_parent_dir: Directory = parent_dir
                    .into_iter()
                    .filter(|item| item.name != &*name)
                    .collect();
                self.save_dir(parent, &new_parent_dir).await?;
                Ok(())
            }
        }
    }

    pub async fn lookup(&self, parent: u64, name: ByteString) -> Result<u64> {
        self.get_index(parent, name.clone())
            .await?
            .ok_or_else(|| FsError::FileNotFound {
                file: name.to_string(),
            })
    }

    pub async fn fallocate(&mut self, inode: &mut Inode, offset: i64, length: i64) -> Result<()> {
        let target_size = (offset + length) as u64;
        if target_size <= inode.size {
            return Ok(());
        }

        if inode.inline_data.is_some() {
            if target_size <= TiFs::INLINE_DATA_THRESHOLD {
                let original_size = inode.size;
                let data = vec![0; (target_size - original_size) as usize];
                self.write_inline_data(inode, original_size, &data).await?;
                return Ok(());
            } else {
                self.transfer_inline_data_to_block(inode).await?;
            }
        }

        inode.set_size(target_size);
        inode.mtime = SystemTime::now();
        self.save_inode(inode).await?;
        Ok(())
    }

    pub async fn mkdir(
        &mut self,
        parent: u64,
        name: ByteString,
        mode: u32,
        gid: u32,
        uid: u32,
    ) -> Result<Inode> {
        let dir_mode = make_mode(FileType::Directory, as_file_perm(mode));
        let attr = self.make_inode(parent, name, dir_mode, gid, uid).await?;
        self.save_dir(attr.ino, &Directory::new()).await?;
        Ok(attr)
    }

    pub async fn read_dir(&mut self, ino: u64) -> Result<Directory> {
        let data = self
            .get(ScopedKey::dir(ino))
            .await?
            .ok_or_else(|| FsError::BlockNotFound {
                inode: ino,
                block: 0,
            })?;
        trace!("read data: {}", String::from_utf8_lossy(&data));
        super::dir::decode(&data)
    }

    pub async fn save_dir(&mut self, ino: u64, dir: &Directory) -> Result<()> {
        let data = super::dir::encode(dir)?;
        let mut attr = self.read_inode(ino).await?;
        attr.set_size(data.len() as u64);
        attr.atime = SystemTime::now();
        attr.mtime = SystemTime::now();
        attr.ctime = SystemTime::now();
        self.save_inode(&attr).await?;
        self.put(ScopedKey::dir(ino), data).await?;
        Ok(())
    }
}

impl Deref for Txn {
    type Target = Transaction;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Txn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
