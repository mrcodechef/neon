//!
//! An in-memory layer stores recently received page versions in memory. The page versions
//! are held in a BTreeMap, and there's another BTreeMap to track the size of the relation.
//!

use crate::layered_repository::storage_layer::Layer;
use crate::layered_repository::storage_layer::PageVersion;
use crate::layered_repository::SnapshotLayer;
use crate::relish::*;
use crate::repository::WALRecord;
use crate::walredo::WalRedoManager;
use crate::PageServerConf;
use crate::{ZTenantId, ZTimelineId};
use anyhow::{bail, Result};
use bytes::Bytes;
use log::*;
use std::collections::BTreeMap;
use std::ops::Bound::Included;
use std::sync::Mutex;

use zenith_utils::lsn::Lsn;

static ZERO_PAGE: Bytes = Bytes::from_static(&[0u8; 8192]);

pub struct InMemoryLayer {
    conf: &'static PageServerConf,
    tenantid: ZTenantId,
    timelineid: ZTimelineId,
    rel: RelishTag,

    ///
    /// This layer contains all the changes from 'start_lsn'. The
    /// start is inclusive. There is no end LSN; we only use in-memory
    /// layer at the end of a timeline.
    ///
    start_lsn: Lsn,

    // FIXME: the three mutex-protected fields below should probably be protected
    // by a single mutex.
    /// If this relation was dropped, remember when that happened. Lsn(0) means
    /// it hasn't been dropped
    drop_lsn: Mutex<Lsn>,

    ///
    /// All versions of all pages in the layer are are kept here.
    /// Indexed by block number and LSN.
    ///
    page_versions: Mutex<BTreeMap<(u32, Lsn), PageVersion>>,

    ///
    /// `relsizes` tracks the size of the relation at different points in time.
    ///
    relsizes: Mutex<BTreeMap<Lsn, u32>>,
}

impl Layer for InMemoryLayer {
    fn is_frozen(&self) -> bool {
        return false;
    }

    fn get_timeline_id(&self) -> ZTimelineId {
        return self.timelineid;
    }

    fn get_relish_tag(&self) -> RelishTag {
        return self.rel;
    }

    fn get_start_lsn(&self) -> Lsn {
        return self.start_lsn;
    }

    fn get_end_lsn(&self) -> Lsn {
        return Lsn(u64::MAX);
    }

    /// Look up given page in the cache.
    fn get_page_at_lsn(
        &self,
        walredo_mgr: &dyn WalRedoManager,
        blknum: u32,
        lsn: Lsn,
    ) -> Result<Bytes> {
        // Scan the BTreeMap backwards, starting from the given entry.
        let mut records: Vec<WALRecord> = Vec::new();
        let mut page_img: Option<Bytes> = None;
        let mut need_base_image_lsn: Option<Lsn> = Some(lsn);
        {
            let page_versions = self.page_versions.lock().unwrap();
            let minkey = (blknum, Lsn(0));
            let maxkey = (blknum, lsn);
            let mut iter = page_versions.range((Included(&minkey), Included(&maxkey)));
            while let Some(((_blknum, entry_lsn), entry)) = iter.next_back() {
                if let Some(img) = &entry.page_image {
                    page_img = Some(img.clone());
                    need_base_image_lsn = None;
                    break;
                } else if let Some(rec) = &entry.record {
                    records.push(rec.clone());
                    if rec.will_init {
                        // This WAL record initializes the page, so no need to go further back
                        need_base_image_lsn = None;
                        break;
                    } else {
                        need_base_image_lsn = Some(*entry_lsn);
                    }
                } else {
                    // No base image, and no WAL record. Huh?
                    bail!("no page image or WAL record for requested page");
                }
            }

            // release lock on 'page_versions'
        }
        records.reverse();

        // If we needed a base image to apply the WAL records against, we should have found it in memory.
        if let Some(lsn) = need_base_image_lsn {
            if records.is_empty() {
                // no records, and no base image. This can happen if PostgreSQL extends a relation
                // but never writes the page.
                //
                // Would be nice to detect that situation better.
                warn!("Page {} blk {} at {} not found", self.rel, blknum, lsn);
                return Ok(ZERO_PAGE.clone());
            }
            bail!(
                "No base image found for page {} blk {} at {}/{}",
                self.rel,
                blknum,
                self.timelineid,
                lsn
            );
        }

        // If we have a page image, and no WAL, we're all set
        if records.is_empty() {
            if let Some(img) = page_img {
                trace!(
                    "found page image for blk {} in {} at {}/{}, no WAL redo required",
                    blknum,
                    self.rel,
                    self.timelineid,
                    lsn
                );
                Ok(img)
            } else {
                // FIXME: this ought to be an error?
                warn!("Page {} blk {} at {} not found", self.rel, blknum, lsn);
                Ok(ZERO_PAGE.clone())
            }
        } else {
            // We need to do WAL redo.
            //
            // If we don't have a base image, then the oldest WAL record better initialize
            // the page
            if page_img.is_none() && !records.first().unwrap().will_init {
                // FIXME: this ought to be an error?
                warn!(
                    "Base image for page {}/{} at {} not found, but got {} WAL records",
                    self.rel,
                    blknum,
                    lsn,
                    records.len()
                );
                Ok(ZERO_PAGE.clone())
            } else {
                if page_img.is_some() {
                    trace!("found {} WAL records and a base image for blk {} in {} at {}/{}, performing WAL redo", records.len(), blknum, self.rel, self.timelineid, lsn);
                } else {
                    trace!("found {} WAL records that will init the page for blk {} in {} at {}/{}, performing WAL redo", records.len(), blknum, self.rel, self.timelineid, lsn);
                }
                let img = walredo_mgr.request_redo(self.rel, blknum, lsn, page_img, records)?;

                self.put_page_image(blknum, lsn, img.clone())?;

                Ok(img)
            }
        }
    }

    /// Get size of the relation at given LSN
    fn get_rel_size(&self, lsn: Lsn) -> Result<u32> {
        // Scan the BTreeMap backwards, starting from the given entry.
        let relsizes = self.relsizes.lock().unwrap();
        let mut iter = relsizes.range((Included(&Lsn(0)), Included(&lsn)));

        if let Some((_entry_lsn, entry)) = iter.next_back() {
            let result = *entry;
            drop(relsizes);
            trace!("get_relsize: {} at {} -> {}", self.rel, lsn, result);
            Ok(result)
        } else {
            bail!(
                "No size found for relfile {:?} at {} in memory",
                self.rel,
                lsn
            );
        }
    }

    /// Does this relation exist at given LSN?
    fn get_rel_exists(&self, lsn: Lsn) -> Result<bool> {
        // Scan the BTreeMap backwards, starting from the given entry.
        let relsizes = self.relsizes.lock().unwrap();

        let mut iter = relsizes.range((Included(&Lsn(0)), Included(&lsn)));

        let result = if let Some((_entry_lsn, _entry)) = iter.next_back() {
            true
        } else {
            false
        };
        Ok(result)
    }

    // Write operations

    /// Common subroutine of the public put_wal_record() and put_page_image() functions.
    /// Adds the page version to the in-memory tree
    fn put_page_version(&self, blknum: u32, lsn: Lsn, pv: PageVersion) -> Result<()> {
        trace!(
            "put_page_version blk {} of {} at {}/{}",
            blknum,
            self.rel,
            self.timelineid,
            lsn
        );
        {
            let mut page_versions = self.page_versions.lock().unwrap();
            let old = page_versions.insert((blknum, lsn), pv);

            if old.is_some() {
                // We already had an entry for this LSN. That's odd..
                warn!(
                    "Page version of rel {:?} blk {} at {} already exists",
                    self.rel, blknum, lsn
                );
            }

            // release lock on 'page_versions'
        }

        // Also update the relation size, if this extended the relation.
        {
            let mut relsizes = self.relsizes.lock().unwrap();
            let mut iter = relsizes.range((Included(&Lsn(0)), Included(&lsn)));

            let oldsize;
            if let Some((_entry_lsn, entry)) = iter.next_back() {
                oldsize = *entry;
            } else {
                oldsize = 0;
                //bail!("No old size found for {} at {}", self.tag, lsn);
            }
            if blknum >= oldsize {
                trace!(
                    "enlarging relation {} from {} to {} blocks",
                    self.rel,
                    oldsize,
                    blknum + 1
                );
                relsizes.insert(lsn, blknum + 1);
            }
        }

        Ok(())
    }

    /// Remember that the relation was truncated at given LSN
    fn put_truncation(&self, lsn: Lsn, relsize: u32) -> anyhow::Result<()> {
        let mut relsizes = self.relsizes.lock().unwrap();
        let old = relsizes.insert(lsn, relsize);

        if old.is_some() {
            // We already had an entry for this LSN. That's odd..
            warn!("Inserting truncation, but had an entry for the LSN already");
        }

        Ok(())
    }

    /// Remember that the relation was truncated at given LSN
    fn put_unlink(&self, lsn: Lsn) -> anyhow::Result<()> {
        let mut drop_lsn = self.drop_lsn.lock().unwrap();

        assert!(*drop_lsn == Lsn(0));
        *drop_lsn = lsn;

        info!("dropped relation {} at {}", self.rel, lsn);

        Ok(())
    }

    ///
    /// Write the this in-memory layer to disk, as a snapshot layer.
    ///
    fn freeze(&self, end_lsn: Lsn) -> Result<()> {
        info!(
            "freezing in memory layer for {} on timeline {} at {}",
            self.rel, self.timelineid, end_lsn
        );

        let page_versions = self.page_versions.lock().unwrap();
        let relsizes = self.relsizes.lock().unwrap();
        let drop_lsn = self.drop_lsn.lock().unwrap();

        // FIXME: we assume there are no modification in-flight, and that there are no
        // changes past 'lsn'.

        let page_versions = page_versions.clone();
        let relsizes = relsizes.clone();

        let dropped = *drop_lsn != Lsn(0);

        let end_lsn = if dropped {
            assert!(*drop_lsn < end_lsn);
            *drop_lsn
        } else {
            end_lsn
        };

        let _snapfile = SnapshotLayer::create(
            self.conf,
            self.timelineid,
            self.tenantid,
            self.rel,
            self.start_lsn,
            end_lsn,
            dropped,
            page_versions,
            relsizes,
        )?;

        Ok(())
    }
}

impl InMemoryLayer {
    ///
    /// Create a new, empty, in-memory layer
    ///
    pub fn create(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        rel: RelishTag,
        start_lsn: Lsn,
    ) -> Result<InMemoryLayer> {
        trace!(
            "initializing new InMemoryLayer for writing {} on timeline {} at {}",
            rel,
            timelineid,
            start_lsn
        );

        Ok(InMemoryLayer {
            conf,
            timelineid,
            tenantid,
            rel,
            start_lsn,
            drop_lsn: Mutex::new(Lsn(0)),
            page_versions: Mutex::new(BTreeMap::new()),
            relsizes: Mutex::new(BTreeMap::new()),
        })
    }

    ///
    /// Initialize a new InMemoryLayer for, by copying the state at the given
    /// point in time from given existing layer.
    ///
    pub fn copy_snapshot(
        conf: &'static PageServerConf,
        walredo_mgr: &dyn WalRedoManager,
        src: &dyn Layer,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        lsn: Lsn,
    ) -> Result<InMemoryLayer> {
        trace!(
            "initializing new InMemoryLayer for writing {} on timeline {} at {}",
            src.get_relish_tag(),
            timelineid,
            lsn
        );
        let mut page_versions = BTreeMap::new();
        let mut relsizes = BTreeMap::new();

        let size = src.get_rel_size(lsn)?;
        relsizes.insert(lsn, size);

        for blknum in 0..size {
            let img = src.get_page_at_lsn(walredo_mgr, blknum, lsn)?;
            let pv = PageVersion {
                page_image: Some(img),
                record: None,
            };
            page_versions.insert((blknum, lsn), pv);
        }

        Ok(InMemoryLayer {
            conf,
            timelineid,
            tenantid,
            rel: src.get_relish_tag(),
            start_lsn: lsn,
            drop_lsn: Mutex::new(Lsn(0)),
            page_versions: Mutex::new(page_versions),
            relsizes: Mutex::new(relsizes),
        })
    }

    /// debugging function to print out the contents of the layer
    #[allow(unused)]
    pub fn dump(&self) -> String {
        let mut result = format!(
            "----- inmemory layer for {} {}-> ----\n",
            self.rel, self.start_lsn
        );

        let relsizes = self.relsizes.lock().unwrap();
        let page_versions = self.page_versions.lock().unwrap();

        for (k, v) in relsizes.iter() {
            result += &format!("{}: {}\n", k, v);
        }
        for (k, v) in page_versions.iter() {
            result += &format!(
                "blk {} at {}: {}/{}\n",
                k.0,
                k.1,
                v.page_image.is_some(),
                v.record.is_some()
            );
        }

        result
    }
}
