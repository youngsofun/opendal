// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use async_compat::Compat;
use async_trait::async_trait;
use futures::io;
use futures::AsyncReadExt;
use futures::AsyncSeekExt;
use futures::AsyncWriteExt;
use log::error;
use log::info;
use metrics::increment_counter;
use tokio::fs;

use super::error::parse_io_error;
use super::object_stream::Readdir;
use crate::error::Error;
use crate::error::Kind;
use crate::error::Result;
use crate::object::BoxedObjectStream;
use crate::object::Metadata;
use crate::object::ObjectMode;
use crate::ops::OpDelete;
use crate::ops::OpList;
use crate::ops::OpRead;
use crate::ops::OpStat;
use crate::ops::OpWrite;
use crate::Accessor;
use crate::BoxedAsyncReader;

#[derive(Default, Debug)]
pub struct Builder {
    root: Option<String>,
}

impl Builder {
    pub fn root(&mut self, root: &str) -> &mut Self {
        self.root = Some(root.to_string());

        self
    }

    pub async fn finish(&mut self) -> Result<Arc<dyn Accessor>> {
        info!("backend build started: {:?}", &self);

        // Make `/` as the default of root.
        let root = match &self.root {
            None => "/".to_string(),
            Some(v) => {
                if !v.starts_with('/') {
                    return Err(Error::Backend {
                        kind: Kind::BackendConfigurationInvalid,
                        context: HashMap::from([("root".to_string(), v.clone())]),
                        source: anyhow!("Root must start with /"),
                    });
                }
                v.to_string()
            }
        };

        // If root dir is not exist, we must create it.
        if let Err(e) = fs::metadata(&root).await {
            if e.kind() == std::io::ErrorKind::NotFound {
                fs::create_dir_all(&root)
                    .await
                    .map_err(|e| parse_io_error(e, "build", &root))?;
            }
        }

        info!("backend build finished: {:?}", &self);
        Ok(Arc::new(Backend { root }))
    }
}

/// Backend is used to serve `Accessor` support for posix alike fs.
///
/// # Note
///
/// We will use separate dedicated thread pool (powered by `unblocking`)
/// for better async performance under tokio. All `std::File` will be wrapped
/// by `Unblock` to gain async support. IO will happen at the separate dedicated
/// thread pool, so we will not block the tokio runtime.
#[derive(Debug, Clone)]
pub struct Backend {
    root: String,
}

impl Backend {
    pub fn build() -> Builder {
        Builder::default()
    }

    pub(crate) fn get_abs_path(&self, path: &str) -> String {
        // Joining an absolute path replaces the existing path, we need to
        // normalize it before.
        let path = path
            .split('/')
            .filter(|v| !v.is_empty())
            .collect::<Vec<&str>>()
            .join("/");

        PathBuf::from(&self.root)
            .join(path)
            .to_string_lossy()
            .to_string()
    }
}

#[async_trait]
impl Accessor for Backend {
    async fn read(&self, args: &OpRead) -> Result<BoxedAsyncReader> {
        increment_counter!("opendal_fs_read_requests");

        let path = self.get_abs_path(&args.path);
        info!(
            "object {} read start: offset {:?}, size {:?}",
            &path, args.offset, args.size
        );

        let f = fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .await
            .map_err(|e| {
                let e = parse_io_error(e, "read", &path);
                error!("object {} open: {:?}", &path, e);
                e
            })?;

        let mut f = Compat::new(f);

        if let Some(offset) = args.offset {
            f.seek(SeekFrom::Start(offset)).await.map_err(|e| {
                let e = parse_io_error(e, "read", &path);
                error!("object {} seek: {:?}", &path, e);
                e
            })?;
        };

        let r: BoxedAsyncReader = match args.size {
            Some(size) => Box::new(f.take(size)),
            None => Box::new(f),
        };

        info!(
            "object {} reader created: offset {:?}, size {:?}",
            &path, args.offset, args.size
        );
        Ok(r)
    }

    async fn write(&self, mut r: BoxedAsyncReader, args: &OpWrite) -> Result<usize> {
        increment_counter!("opendal_fs_write_requests");

        let path = self.get_abs_path(&args.path);
        info!("object {} write start: size {}", &path, args.size);

        // Create dir before write path.
        //
        // TODO(xuanwo): There are many works to do here:
        //   - Is it safe to create dir concurrently?
        //   - Do we need to extract this logic as new util functions?
        //   - Is it better to check the parent dir exists before call mkdir?
        let parent = PathBuf::from(&path)
            .parent()
            .ok_or_else(|| anyhow!("malformed path: {:?}", &path))?
            .to_path_buf();

        fs::create_dir_all(&parent).await.map_err(|e| {
            let e = parse_io_error(e, "write", &parent.to_string_lossy());
            error!(
                "object {} create_dir_all for parent {}: {:?}",
                &path,
                &parent.to_string_lossy(),
                e
            );
            e
        })?;

        let f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .await
            .map_err(|e| {
                let e = parse_io_error(e, "write", &path);
                error!("object {} open: {:?}", &path, e);
                e
            })?;

        let mut f = Compat::new(f);

        // TODO: we should respect the input size.
        let s = io::copy(&mut r, &mut f).await.map_err(|e| {
            let e = parse_io_error(e, "write", &path);
            error!("object {} copy: {:?}", &path, e);
            e
        })?;

        // `std::fs::File`'s errors detected on closing are ignored by
        // the implementation of Drop.
        // So we need to call `flush` to make sure all data have been flushed
        // to fs successfully.
        f.flush().await.map_err(|e| {
            let e = parse_io_error(e, "write", &path);
            error!("object {} flush: {:?}", &path, e);
            e
        })?;

        info!("object {} write finished: size {:?}", &path, args.size);
        Ok(s as usize)
    }

    async fn stat(&self, args: &OpStat) -> Result<Metadata> {
        increment_counter!("opendal_fs_stat_requests");

        let path = self.get_abs_path(&args.path);
        info!("object {} stat start", &path);

        let meta = fs::metadata(&path).await.map_err(|e| {
            let e = parse_io_error(e, "stat", &path);
            error!("object {} stat: {:?}", &path, e);
            e
        })?;

        let mut m = Metadata::default();
        m.set_path(&args.path);
        if meta.is_dir() {
            m.set_mode(ObjectMode::DIR);
        } else {
            // TODO: we should handle LINK or other types here.
            m.set_mode(ObjectMode::FILE);
        }
        m.set_content_length(meta.len() as u64);
        m.set_last_modified(
            meta.modified()
                .map_err(|e| parse_io_error(e, "stat", &path))?,
        );
        m.set_complete();

        info!("object {} stat finished", &path);
        Ok(m)
    }

    async fn delete(&self, args: &OpDelete) -> Result<()> {
        increment_counter!("opendal_fs_delete_requests");

        let path = self.get_abs_path(&args.path);
        info!("object {} delete start", &path);

        // PathBuf.is_dir() is not free, call metadata directly instead.
        let meta = fs::metadata(&path).await;

        if let Err(err) = meta {
            return if err.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                let e = parse_io_error(err, "delete", &path);
                error!("object {} delete: {:?}", &path, e);
                Err(e)
            };
        }

        // Safety: Err branch has been checked, it's OK to unwrap.
        let meta = meta.ok().unwrap();

        let f = if meta.is_dir() {
            fs::remove_dir(&path).await
        } else {
            fs::remove_file(&path).await
        };

        f.map_err(|e| parse_io_error(e, "delete", &path))?;

        info!("object {} delete finished", &path);
        Ok(())
    }

    async fn list(&self, args: &OpList) -> Result<BoxedObjectStream> {
        increment_counter!("opendal_fs_list_requests");

        let path = self.get_abs_path(&args.path);
        info!("object {} list start", &path);

        let f = fs::read_dir(&path).await.map_err(|e| {
            let e = parse_io_error(e, "read", &path);
            error!("object {} list: {:?}", &path, e);
            e
        })?;

        let rd = Readdir::new(Arc::new(self.clone()), &self.root, &args.path, f);

        Ok(Box::new(rd))
    }
}
