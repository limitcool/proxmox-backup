use std::io::Read;
use std::path::Path;

use anyhow::{bail, Error};

use crate::api2::types::{RRDMode, RRDTimeFrameResolution};

pub const RRD_DATA_ENTRIES: usize = 70;

use bitflags::bitflags;

bitflags!{
    pub struct RRAFlags: u64 {
        // Data Source Types
        const DST_GAUGE  = 1;
        const DST_DERIVE = 2;
        const DST_MASK   = 255; // first 8 bits

        // Consolidation Functions
        const CF_AVERAGE = 1 << 8;
        const CF_MAX     = 2 << 8;
        const CF_MASK    = 255 << 8;
    }
}

pub enum DST {
    Gauge,
    Derive,
}

#[repr(C)]
struct RRA {
    flags: RRAFlags,
    resolution: u64,
    last_update: u64,
    last_count: u64,
    data: [f64; RRD_DATA_ENTRIES],
}

impl RRA {
    fn new(flags: RRAFlags, resolution: u64) -> Self {
        Self {
            flags, resolution,
            last_update: 0,
            last_count: 0,
            data: [f64::NAN; RRD_DATA_ENTRIES],
        }
    }

    fn delete_old(&mut self, epoch: u64) {
        let reso = self.resolution;
        let min_time = epoch - (RRD_DATA_ENTRIES as u64)*reso;
        let min_time = (min_time/reso + 1)*reso;
        let mut t = self.last_update - (RRD_DATA_ENTRIES as u64)*reso;
        let mut index = ((t/reso) % (RRD_DATA_ENTRIES as u64)) as usize;
        for _ in 0..RRD_DATA_ENTRIES {
            t += reso; index = (index + 1) % RRD_DATA_ENTRIES;
            if t < min_time {
                self.data[index] = f64::NAN;
            } else {
                break;
            }
        }
    }

    fn compute_new_value(&mut self, epoch: u64, value: f64) {
        let reso = self.resolution;
        let index = ((epoch/reso) % (RRD_DATA_ENTRIES as u64)) as usize;
        let last_index = ((self.last_update/reso) % (RRD_DATA_ENTRIES as u64)) as usize;

        if (epoch - self.last_update) > reso || index != last_index {
            self.last_count = 0;
        }

        let last_value = self.data[index];
        if last_value.is_nan() {
            self.last_count = 0;
        }

        let new_count = self.last_count + 1; // fixme: check overflow?
        if self.last_count == 0 {
            self.data[index] = value;
            self.last_count = 1;
        } else {
            let new_value = if self.flags.contains(RRAFlags::CF_MAX) {
                if last_value > value { last_value } else { value }
            } else if self.flags.contains(RRAFlags::CF_AVERAGE) {
                (last_value*(self.last_count as f64))/(new_count as f64)
                    + value/(new_count as f64)
            } else {
                eprintln!("rrdb update failed - unknown CF");
                return;
            };
            self.data[index] = new_value;
            self.last_count = new_count;
        }
        self.last_update = epoch;
    }

    fn update(&mut self, epoch: u64, value: f64) {
        if epoch < self.last_update {
            eprintln!("rrdb update failed - time in past ({} < {})", epoch, self.last_update);
        }
        if value.is_nan() {
            eprintln!("rrdb update failed - new value is NAN");
            return;
        }

        self.delete_old(epoch);
        self.compute_new_value(epoch, value);
    }
}

#[repr(C)]
// Note: Avoid alignment problems by using 8byte types only
pub struct RRD {
    hour_avg: RRA,
    hour_max: RRA,
    day_avg: RRA,
    day_max: RRA,
    week_avg: RRA,
    week_max: RRA,
    month_avg: RRA,
    month_max: RRA,
    year_avg: RRA,
    year_max: RRA,
}

impl RRD {

    pub fn new(dst: DST) -> Self {
        let flags = match dst {
            DST::Gauge => RRAFlags::DST_GAUGE,
            DST::Derive => RRAFlags::DST_DERIVE,
        };

        Self {
            hour_avg: RRA::new(
                flags | RRAFlags::CF_AVERAGE,
                RRDTimeFrameResolution::Hour as u64,
            ),
            hour_max: RRA::new(
                flags |  RRAFlags::CF_MAX,
                RRDTimeFrameResolution::Hour as u64,
            ),
            day_avg: RRA::new(
                flags |  RRAFlags::CF_AVERAGE,
                RRDTimeFrameResolution::Day as u64,
            ),
            day_max: RRA::new(
                flags |  RRAFlags::CF_MAX,
                RRDTimeFrameResolution::Day as u64,
            ),
            week_avg: RRA::new(
                flags |  RRAFlags::CF_AVERAGE,
                RRDTimeFrameResolution::Week as u64,
            ),
            week_max: RRA::new(
                flags |  RRAFlags::CF_MAX,
                RRDTimeFrameResolution::Week as u64,
            ),
            month_avg: RRA::new(
                flags |  RRAFlags::CF_AVERAGE,
                RRDTimeFrameResolution::Month as u64,
            ),
            month_max: RRA::new(
                flags |  RRAFlags::CF_MAX,
                RRDTimeFrameResolution::Month as u64,
            ),
            year_avg: RRA::new(
                flags |  RRAFlags::CF_AVERAGE,
                RRDTimeFrameResolution::Year as u64,
            ),
            year_max: RRA::new(
                flags |  RRAFlags::CF_MAX,
                RRDTimeFrameResolution::Year as u64,
            ),
        }
    }

    pub fn extract_data(
        &self,
        epoch: u64,
        timeframe: RRDTimeFrameResolution,
        mode: RRDMode,
    ) -> (u64, u64, Vec<Option<f64>>) {

        let reso = timeframe as u64;

        let end = reso*(epoch/reso + 1);
        let start = end - reso*(RRD_DATA_ENTRIES as u64);

        let mut list = Vec::new();

        let raa = match (mode, timeframe) {
            (RRDMode::Average, RRDTimeFrameResolution::Hour) => &self.hour_avg,
            (RRDMode::Max, RRDTimeFrameResolution::Hour) => &self.hour_max,
            (RRDMode::Average, RRDTimeFrameResolution::Day) => &self.day_avg,
            (RRDMode::Max, RRDTimeFrameResolution::Day) => &self.day_max,
            (RRDMode::Average, RRDTimeFrameResolution::Week) => &self.week_avg,
            (RRDMode::Max, RRDTimeFrameResolution::Week) => &self.week_max,
            (RRDMode::Average, RRDTimeFrameResolution::Month) => &self.month_avg,
            (RRDMode::Max, RRDTimeFrameResolution::Month) => &self.month_max,
            (RRDMode::Average, RRDTimeFrameResolution::Year) => &self.year_avg,
            (RRDMode::Max, RRDTimeFrameResolution::Year) => &self.year_max,
        };

        let rrd_end = reso*(raa.last_update/reso);
        let rrd_start = rrd_end - reso*(RRD_DATA_ENTRIES as u64);

        let mut t = start;
        let mut index = ((t/reso) % (RRD_DATA_ENTRIES as u64)) as usize;
        for _ in 0..RRD_DATA_ENTRIES {
            if t < rrd_start || t > rrd_end {
                list.push(None);
            } else {
                let value = raa.data[index];
                if value.is_nan() {
                    list.push(None);
                } else {
                    list.push(Some(value));
                }
            }
            t += reso; index = (index + 1) % RRD_DATA_ENTRIES;
        }

        (start, reso, list.into())
    }

    pub fn from_raw(mut raw: &[u8]) -> Result<Self, Error> {
        let expected_len = std::mem::size_of::<RRD>();
        if raw.len() != expected_len {
            bail!("RRD::from_raw failed - wrong data size ({} != {})", raw.len(), expected_len);
        }

        let mut rrd: RRD = unsafe { std::mem::zeroed() };
        unsafe {
            let rrd_slice = std::slice::from_raw_parts_mut(&mut rrd as *mut _ as *mut u8, expected_len);
            raw.read_exact(rrd_slice)?;
        }

        Ok(rrd)
    }

    pub fn load(filename: &Path) -> Result<Self, Error> {
        let raw = proxmox::tools::fs::file_get_contents(filename)?;
        Self::from_raw(&raw)
    }

    pub fn save(&self, filename: &Path) -> Result<(), Error> {
        use proxmox::tools::{fs::replace_file, fs::CreateOptions};

        let rrd_slice = unsafe {
            std::slice::from_raw_parts(self as *const _ as *const u8, std::mem::size_of::<RRD>())
        };

        let backup_user = crate::backup::backup_user()?;
        let mode = nix::sys::stat::Mode::from_bits_truncate(0o0644);
        // set the correct owner/group/permissions while saving file
        // owner(rw) = backup, group(r)= backup
        let options = CreateOptions::new()
            .perm(mode)
            .owner(backup_user.uid)
            .group(backup_user.gid);

        replace_file(filename, rrd_slice, options)?;

        Ok(())
    }


    pub fn update(&mut self, epoch: u64, value: f64) {
        self.hour_avg.update(epoch, value);
        self.hour_max.update(epoch, value);

        self.day_avg.update(epoch, value);
        self.day_max.update(epoch, value);

        self.week_avg.update(epoch, value);
        self.week_max.update(epoch, value);

        self.month_avg.update(epoch, value);
        self.month_max.update(epoch, value);

        self.year_avg.update(epoch, value);
        self.year_max.update(epoch, value);
    }
}
