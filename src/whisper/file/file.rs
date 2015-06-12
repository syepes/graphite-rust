use std::fs::File;
use std::io::{SeekFrom, Seek, Read, Write};
use std::fs::OpenOptions;
use std::fmt;
use num::iter::{ range_step_inclusive, RangeStepInclusive };
use std::cell::RefCell;
use std::io::Error;

extern crate libc;
use self::libc::funcs::posix01::unistd::ftruncate;
use std::os::unix::prelude::AsRawFd;

use super::header::{ Header, read_header };
use super::write_op::WriteOp;
use super::archive_info::ArchiveInfo;
use super::metadata::{Metadata, AggregationType};
use whisper::schema::Schema;

use whisper::point;

pub struct WhisperFile {
    pub handle: RefCell<File>,
    pub header: Header
}

impl fmt::Debug for WhisperFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ref metadata = self.header.metadata;
        let ref archive_infos = self.header.archive_infos;

        try!(writeln!(f, "whisper file"));
        try!(writeln!(f, "  metadata"));
        try!(writeln!(f, "    aggregation method: {:?}", metadata.aggregation_type));
        try!(writeln!(f, "    max retention: {:?}", metadata.max_retention));
        try!(writeln!(f, "    xff: {:?}", metadata.x_files_factor));

        for (index,archive_info) in (0..).zip(archive_infos.iter()) {
            // Archive details
            try!(writeln!(f, "  archive {}", index));
            try!(writeln!(f, "    offset: {}", archive_info.offset));
            try!(writeln!(f, "    seconds per point: {}", archive_info.seconds_per_point));
            try!(writeln!(f, "    points: {}", archive_info.points));
            try!(writeln!(f, "    retention: {} (s)", archive_info.retention));
            try!(write!(f, "    size: {} (bytes)\n", archive_info.size_in_bytes()));

            // Print out all the data from this archive
            try!(writeln!(f, "    data"));

            let mut points : Vec<point::Point> = vec![point::Point{timestamp: 0, value: 0.0}; archive_info.points as usize];
            self.read_points(archive_info.offset, &mut points[..]);
            for point in points {
                try!(writeln!(f, "      timestamp: {} value: {}", point.timestamp, point.value));
            }

            if index != archive_infos.len() - 1 {
                try!(writeln!(f, ""));
            }
        }
        write!(f,"") // make the types happy
    }
}

impl fmt::Display for WhisperFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ref metadata = self.header.metadata;
        let ref archive_infos = self.header.archive_infos;

        try!(writeln!(f, "whisper file"));
        try!(writeln!(f, "  metadata"));
        try!(writeln!(f, "    aggregation method: {:?}", metadata.aggregation_type));
        try!(writeln!(f, "    max retention: {:?}", metadata.max_retention));
        try!(writeln!(f, "    xff: {:?}", metadata.x_files_factor));

        for (index,archive_info) in (0..).zip(archive_infos.iter()) {
            try!(writeln!(f, "  archive {}", index));
            try!(writeln!(f, "    offset: {}", archive_info.offset));
            try!(writeln!(f, "    seconds per point: {}", archive_info.seconds_per_point));
            try!(writeln!(f, "    points: {}", archive_info.points));
            try!(writeln!(f, "    retention: {} (s)", archive_info.retention));
            try!(write!(f, "    size: {} (bytes)\n", archive_info.size_in_bytes()));

            if index != archive_infos.len() - 1 {
                try!(writeln!(f, ""));
            }
        }
        write!(f,"") // make the types happy
    }
}

pub fn open(path: &str) -> Result<WhisperFile, Error> {
    let file = try!(OpenOptions::new().read(true).write(true)
                        .create(false).open(path));

    let header = try!(read_header(&file));
    let whisper_file = WhisperFile { header: header, handle: RefCell::new(file) };

    Ok(whisper_file)
}

impl WhisperFile {

    pub fn new(path: &str, schema: Schema /* , _: Metadata */) -> Result<WhisperFile, Error> {
        let opened_file = try!(OpenOptions::new().read(true).write(true).create(true).open(path));
        WhisperFile::new_from_file(opened_file, schema)
    }

    pub fn new_from_file(opened_file: File, schema: Schema) -> Result<WhisperFile, Error> {
        let size_needed = schema.size_on_disk();

        // Allocate the room necessary
        debug!("allocating {} bytes...", size_needed);
        {
            let raw_fd = opened_file.as_raw_fd();
            let retval = unsafe {
                // TODO skip to fallocate-like behavior. Will need wrapper for OSX.
                ftruncate(raw_fd, size_needed as i64)
            };
            if retval != 0 {
                return Err(Error::last_os_error());
            }
        }
        debug!("done allocating");

        let metadata = {
            // TODO make agg_t, max_r options from the command line.
            let aggregation_type = AggregationType::Average;
            let x_files_factor = 0.5;
            Metadata {
                aggregation_type: aggregation_type,
                max_retention: schema.max_retention() as u32,
                x_files_factor: x_files_factor,
                archive_count: schema.retention_policies.len() as u32
            }
        };

        // Piggy back on moving file write forward
        metadata.write(&opened_file);

        let mut archive_offset = schema.header_size_on_disk();

        // write the archive info to disk and build ArchiveInfos
        let archive_infos : Vec<ArchiveInfo> = schema.retention_policies.iter().map(|&rp| {
            rp.write(&opened_file, archive_offset);
            let archive_info = ArchiveInfo {
                offset: archive_offset,
                seconds_per_point: rp.precision,
                retention: rp.retention,
                points: rp.points()
            };
            archive_offset = archive_offset + rp.size_on_disk();
            archive_info
        }).collect();

        let new_whisper_file = WhisperFile {
            handle: RefCell::new(opened_file),
            header: Header {
                metadata: metadata,
                archive_infos: archive_infos
            }
        };
        Ok(new_whisper_file)
    }

    // TODO: Result<usize> return how many write ops were done
    pub fn write(&mut self, current_time: u64, point: point::Point) {

        match self.split(current_time, point.timestamp) {
            Some( (high_precision_archive, rest) ) => {
                let base_point = self.read_point(high_precision_archive.offset);
                let base_timestamp = base_point.timestamp;

                self.write_archives(
                    (high_precision_archive, rest),
                    point,
                    base_timestamp
                );
            },
            None => {
                panic!("no archives satisfy current time")
            }
        }

    }

    fn perform_write_op(&self, write_op: &WriteOp) {
        let mut handle = self.handle.borrow_mut();
        handle.seek(write_op.seek).unwrap();
        handle.write_all(&(write_op.bytes)).unwrap();
    }

    fn read_point(&self, offset: u64) -> point::Point {
        let mut file = self.handle.borrow_mut();
        file.seek(SeekFrom::Start(offset)).unwrap();

        let mut points_buf : [u8; 12] = [0; 12];
        let mut buf_ref : &mut [u8] = &mut points_buf;
        file.read(buf_ref).unwrap();

        point::buf_to_point(buf_ref)
    }

    // Attempt at a weird API: you pass me a slice and I fill it with points.
    fn read_points(&self, offset: u64, points: &mut [point::Point]) {
        let mut points_buf = vec![0; points.len() * point::POINT_SIZE];

        let mut file = self.handle.borrow_mut();
        file.seek(SeekFrom::Start(offset)).unwrap();
        let bytes_read = file.read(&mut points_buf[..]).unwrap();
        assert_eq!(bytes_read, points_buf.len());

        let buf_chunks = points_buf.chunks(point::POINT_SIZE);
        let index_chunk_pairs = (0..points.len()).zip(buf_chunks);

        for (index,chunk) in index_chunk_pairs {
            points[index] = point::buf_to_point(chunk);
        }
    }

    fn write_archives(&self, (ai,rest): (&ArchiveInfo, Vec<&ArchiveInfo>), point: point::Point, base_timestamp: u64) {
        {
            let write_op = build_write_op( ai, &point, base_timestamp );
            self.perform_write_op(&write_op);
        }

        if rest.len() > 0 {
            self.downsample_new(ai, rest[0], point.timestamp).map(|write_op| self.perform_write_op(&write_op) );

            let high_res_iter = rest[0..rest.len()-1].into_iter();
            let low_res_iter = rest[1..].into_iter();
            let _ : Vec<()> = high_res_iter.zip(low_res_iter).
                take_while(|&(h,l)| {
                    match self.downsample_new(h, l, point.timestamp) {
                        Some(write_op) => {
                            self.perform_write_op(&write_op);
                            true
                        },
                        None => false
                    }
                }).
                map(|_| ()).
                collect();
        }
    }

    // TODO convert to return value to Result<WriteOp> so we can log why an update couldn't be done
    fn downsample_new(&self, h_res_archive: &ArchiveInfo, l_res_archive: &ArchiveInfo, base_timestamp: u64) -> Option<WriteOp> {
        // allocate space for all necessary points from higher archive
        let mut h_res_points = {
            let h_res_points_needed = l_res_archive.seconds_per_point / h_res_archive.seconds_per_point;
            vec![point::Point{timestamp: 0, value: 0.0}; h_res_points_needed as usize]
        };

        {
            // plan reads
            let reads = self.downsample_new_read_ops(
                h_res_archive, l_res_archive,
                &mut h_res_points[..],
                base_timestamp
            );

            // perform reads
            {
                let ((first_index, first_buf), second_read) = reads;
                {
                    if(h_res_archive.points == 60) {
                        panic!("sup");
                    }

                    let file = self.handle.borrow_mut();
                    h_res_archive.read_points(first_index, first_buf, file);
                }

                match second_read {
                    Some((second_index, second_buf)) => {
                        let file = self.handle.borrow_mut();
                        h_res_archive.read_points(second_index, second_buf, file);
                    },
                    None => ()
                }
            }
        }

        // filter out of date samples
        let total_possible_values = h_res_points.len();
        let filtered_values = {
            let expected_timestamps = {
                let timestamp_start = l_res_archive.interval_ceiling(base_timestamp);
                let timestamp_stop = timestamp_start + (h_res_points.len() as u64)*h_res_archive.seconds_per_point;
                let step = h_res_archive.seconds_per_point;

                range_step_inclusive(timestamp_start, timestamp_stop, step)
            };
            self.filter_values(h_res_points, expected_timestamps)
        };

        // perform aggregation
        let aggregated_value = self.aggregate_samples_consume(filtered_values, total_possible_values as u64);
        aggregated_value.map(|aggregate| {
            let l_interval_start = l_res_archive.interval_ceiling(base_timestamp);
            let l_res_base_point = self.read_point(l_res_archive.offset);
            let l_res_point = point::Point{ timestamp: l_interval_start, value: aggregate };
            build_write_op(l_res_archive, &l_res_point, l_res_base_point.timestamp)
        })

        // write data
    }

    fn filter_values(&self, points: Vec<point::Point>, range: RangeStepInclusive<u64>) -> Vec<point::Point> {
        let filtered_values : Vec<point::Point> = points.into_iter().zip(range).
            filter_map(|(point, expected_timestamp)| {
                if point.timestamp == expected_timestamp {
                    Some(point)
                } else {
                    None
                }
            }).collect();
        filtered_values
    }

    fn downsample_new_read_ops<'a> (&'a self, h_res_archive: &ArchiveInfo, l_res_archive: &ArchiveInfo, h_res_points: &'a mut [point::Point], base_timestamp: u64) -> ((u64, &mut [point::Point]), Option<(u64, &mut [point::Point])>) {
        let h_res_start_index = {
            let h_base_timestamp = self.read_point(h_res_archive.offset).timestamp;

            if h_base_timestamp == 0 {
                0
            } else {
                let l_interval_start = l_res_archive.interval_ceiling(base_timestamp);
                // TODO: this can be negative. Does that change timestamp understanding?
                let timespan  = l_interval_start as i64 - h_base_timestamp as i64;
                let points = timespan / h_res_archive.seconds_per_point as i64;

                // TODO: Work around for modulo not working the same as in python.
                // TODO: OMG, move this craziness somewhere else
                let wrapped_index = {
                    let remainder = points % h_res_archive.points as i64;
                    if remainder < 0 {
                        h_res_archive.points as i64 + remainder
                    } else {
                        remainder
                    }
                };
                wrapped_index as u64
            }
        };

        let h_res_end_index = {
            let h_res_points_needed = l_res_archive.seconds_per_point / h_res_archive.seconds_per_point;
            (h_res_start_index + h_res_points_needed) % h_res_archive.points
        };

        // Contiguous read. The easy one.
        if h_res_start_index < h_res_end_index {
            ((h_res_start_index, &mut h_res_points[..]), None)
        // Wrap-around read
        } else {
            let (first_buf, second_buf) = h_res_points.split_at_mut((h_res_archive.points - h_res_start_index) as usize);
            ((h_res_start_index,first_buf), Some((h_res_end_index, second_buf)))
        }
    }

    // The most expensive IO functionality
    // Reads many samples from high-res archive and
    // aggregates to lower-res archive. Schemas could do well to avoid
    // aggregation unless disk space is truly at a premium.
    //
    // A cache for each archive would do well here. `memmap` would be awesomesauce.
    fn downsample(&self, h_res_archive: &ArchiveInfo, l_res_archive: &ArchiveInfo, base_timestamp: u64) -> Option<WriteOp> {
        assert!(h_res_archive.seconds_per_point < l_res_archive.seconds_per_point);

        let l_interval_start = l_res_archive.interval_ceiling(base_timestamp);

        let h_base_timestamp = self.read_point(h_res_archive.offset).timestamp;
        let h_res_start_offset : u64 = if h_base_timestamp == 0 {
            h_res_archive.offset
        } else {
            // TODO: this can be negative. Does that change timestamp understanding?
            let timespan  = l_interval_start as i64 - h_base_timestamp as i64;
            let points = timespan / h_res_archive.seconds_per_point as i64;
            let bytes = points * point::POINT_SIZE as i64;

            // TODO: Work around for modulo not working the same as in python.
            // TODO: OMG, move this craziness somewhere else
            let wrapped_index = {
                let remainder = bytes % h_res_archive.size_in_bytes() as i64;
                if remainder < 0 {
                    h_res_archive.size_in_bytes() as i64 + remainder
                } else {
                    remainder
                }
            };
            (h_res_archive.offset as i64 + wrapped_index) as u64
        };

        let h_res_points_needed = l_res_archive.seconds_per_point / h_res_archive.seconds_per_point;
        let h_res_bytes_needed = h_res_points_needed * point::POINT_SIZE as u64;

        let h_res_end_offset = {
            let rel_first_offset = h_res_start_offset - h_res_archive.offset;
            let rel_second_offset = (rel_first_offset + h_res_bytes_needed) % h_res_archive.size_in_bytes();
            h_res_archive.offset + rel_second_offset
        };

        let mut h_res_read_buf = vec![0; h_res_bytes_needed as usize];

        // Subroutine for filling in the buffer
        {
            let mut handle = self.handle.borrow_mut();

            // TODO: refactor in to function which
            // returns ((Seek,BytesRead),Option<(Seek,BytesRead)>)
            // so this code can be refactored and unit tested...
            if h_res_start_offset < h_res_end_offset {
                // No wrap situation
                let seek = SeekFrom::Start(h_res_start_offset);

                let mut read_buf : &mut [u8] = &mut h_res_read_buf[..];
                handle.seek(seek).unwrap();
                handle.read(read_buf).unwrap();
            } else {
                let high_res_abs_end = h_res_archive.offset + h_res_archive.size_in_bytes();
                let first_seek = SeekFrom::Start(h_res_start_offset);
                let first_seek_bytes = high_res_abs_end - h_res_start_offset;

                // How cool is that? Guarantee there won't be overlap in buffers borrowed from same array.
                let (first_buf, second_buf) = h_res_read_buf.split_at_mut(first_seek_bytes as usize);

                handle.seek(first_seek).unwrap();
                handle.read(first_buf).unwrap();

                let second_seek = SeekFrom::Start(h_res_archive.offset);
                handle.seek(second_seek).unwrap();
                handle.read(second_buf).unwrap();
            }

        }

        let low_res_aggregate = {
            let points : Vec<point::Point> = h_res_read_buf.chunks(point::POINT_SIZE).map(|chunk| {
                point::buf_to_point(chunk)

            }).collect();

            let timestamp_start = l_interval_start;
            let timestamp_stop = l_interval_start + (h_res_points_needed as u64)*h_res_archive.seconds_per_point;
            let step = h_res_archive.seconds_per_point;

            let expected_timestamps = range_step_inclusive(timestamp_start, timestamp_stop, step);
            let valid_points : Vec<&point::Point> = expected_timestamps.
                zip(points.iter()).
                map(|(ts, p)| {
                    if p.timestamp == ts {
                        Some(p)
                    } else {
                        None
                    }
                }).filter(|agg| !agg.is_none()).map(|agg| agg.unwrap()).collect();
            self.aggregate_samples(valid_points, h_res_points_needed)
        };

        low_res_aggregate.map(|aggregate| {
            let l_res_base_point = self.read_point(l_res_archive.offset);
            let l_res_point = point::Point{ timestamp: l_interval_start, value: aggregate };
            build_write_op(l_res_archive, &l_res_point, l_res_base_point.timestamp)
        })
    }

    fn aggregate_samples_consume(&self, valid_points: Vec<point::Point>, points_possible: u64) -> Option<f64>{
        let ratio : f32 = valid_points.len() as f32 / points_possible as f32;
        if ratio < self.header.metadata.x_files_factor {
            return None;
        }

        // TODO: we only do aggregation right now!
        match self.header.metadata.aggregation_type {
            AggregationType::Average => {
                let valid_points_len = valid_points.len();
                let sum = valid_points.into_iter().map(|p| p.value).fold(0.0, |l, r| l + r);
                Some(sum / valid_points_len as f64)
            },
            _ => { Some(0.0) }
        }
    }

    // TODO remove with old downsample
    fn aggregate_samples(&self, points: Vec<&point::Point>, points_possible: u64) -> Option<f64>{
        let valid_points : Vec<&&point::Point> = points.iter().filter(|p| p.timestamp != 0).map(|p| p).collect();

        let ratio : f32 = valid_points.len() as f32 / points_possible as f32;
        if ratio < self.header.metadata.x_files_factor {
            return None;
        }

        // TODO: we only do aggregation right now!
        match self.header.metadata.aggregation_type {
            AggregationType::Average => {
                let sum = points.iter().map(|p| p.value).fold(0.0, |l, r| l + r);
                Some(sum / points.len() as f64)
            },
            _ => { Some(0.0) }
        }
    }

    // TODO: don't create new vectors, borrow slice from archive_infos
    fn split(&self, current_time: u64, point_timestamp: u64) -> Option<(&ArchiveInfo, Vec<&ArchiveInfo>)>  {
        let mut archive_iter = self.header.archive_infos.iter();
        
        let high_precision_archive_option = archive_iter.find(|ai| {
            ai.retention > (current_time - point_timestamp)
        });

        match high_precision_archive_option {
            Some(ai) => {
                let rest_of_archives = archive_iter;
                let low_res_archives : Vec<&ArchiveInfo> = rest_of_archives.collect();

                Some((ai, low_res_archives))
            },
            None => {
                None
            }
        }
    }
}

fn build_write_op(archive_info: &ArchiveInfo, point: &point::Point, base_timestamp: u64) -> WriteOp {
    let mut output_data = [0; 12];
    let interval_ceiling = archive_info.interval_ceiling(point.timestamp);
    {
        let point_value = point.value;
        let buf : &mut [u8] = &mut output_data;
        point::fill_buf(buf, interval_ceiling, point_value);
    }

    let seek_info = archive_info.calculate_seek(&point,  base_timestamp);

    return WriteOp {
        seek: seek_info,
        bytes: output_data
    }
}

#[cfg(test)]
mod tests {
    use test::Bencher;
    extern crate time;

    use super::super::archive_info::ArchiveInfo;
    use super::{ WhisperFile, build_write_op, open };
    use whisper::point::Point;
    use whisper::schema::{ Schema, RetentionPolicy };
    use whisper::file::metadata::{ Metadata, AggregationType };
    use whisper::file::header::Header;

    fn build_60_1440_wsp(prefix: &str) -> WhisperFile {
        let path = format!("test/fixtures/{}.wsp", prefix);
        let schema = Schema {
            retention_policies: vec![
                RetentionPolicy {
                    precision: 60,
                    retention: 1440
                }
            ]
        };

        WhisperFile::new(&path[..], schema).unwrap()
    }

    fn build_60_1440_1440_168_10080_52(prefix: &str) -> WhisperFile {
        let path = format!("test/fixtures/{}.wsp", prefix);
        let specs = vec![
            "1m:1h".to_string(),
            "1h:1w".to_string(),
            "1w:1y".to_string()
        ];
        let schema = Schema::new_from_retention_specs(specs);

        WhisperFile::new(&path[..], schema).unwrap()
    }

    // #[bench]
    // fn bench_build_write_op(b: &mut Bencher) {
    //     let archive_info = ArchiveInfo {
    //         offset: 28,
    //         seconds_per_point: 60,
    //         points: 1000,
    //         retention: 10000
    //     };
    //     let point = Point {
    //         timestamp: 1000,
    //         value: 10.0
    //     };
    //     let base_timestamp = 900;

    //     b.iter(|| build_write_op(&archive_info, &point, base_timestamp));
    // }

    // #[bench]
    // fn bench_opening_a_file(b: &mut Bencher) {
    //     let path = "test/fixtures/60-1440.wsp";
    //     // TODO: how is this so fast? 7ns seems crazy. caching involved?
    //     b.iter(|| open(path).unwrap() );
    // }

    // #[bench]
    // fn bench_writing_through_a_small_file(b: &mut Bencher) {
    //     let mut whisper_file = build_60_1440_wsp("small_file");
    //     let current_time = time::get_time().sec as u64;

    //     b.iter(|| {
    //         let point = Point {
    //             timestamp: current_time,
    //             value: 10.0
    //         };
    //         whisper_file.write(current_time, point);
    //     });
    // }

    #[bench]
    fn bench_writing_through_a_large_file(b: &mut Bencher) {
        let mut whisper_file = build_60_1440_1440_168_10080_52("a_large_file");
        let current_time = time::get_time().sec as u64;

        b.iter(|| {
            let point = Point {
                timestamp: current_time,
                value: 10.0
            };
            whisper_file.write(current_time, point);
        });
    }

    // #[test]
    // fn test_read_point() {
    //     let file = open("test/fixtures/60-1440.wsp").unwrap();
    //     let offset = file.header.archive_infos[0].offset;
    //     // read the first point of the first archive
    //     let point = file.read_point(offset);
    //     assert_eq!(point, Point{timestamp: 0, value: 0.0});
    // }

    // #[test]
    // fn test_read_points() {
    //     let file = open("test/fixtures/60-1440.wsp").unwrap();
    //     let offset = file.header.archive_infos[0].offset;
    //     // read the first point of the first archive

    //     let mut points_holder : Vec<Point> = vec![ Point{ timestamp: 0, value: 0.0 }; 10 ];
    //     file.read_points(offset, &mut points_holder[..]);

    //     let expected = vec![Point{timestamp: 0, value: 0.0}; points_holder.len()];
    //     assert_eq!(points_holder, expected);
    // }

    // #[test]
    // fn test_new_file_has_correct_metadata() {
    //     let specs = vec![
    //         "1m:1h".to_string(),
    //         "1h:1w".to_string(),
    //         "1w:1y".to_string()
    //     ];
    //     let schema = Schema::new_from_retention_specs(specs);

    //     let file = WhisperFile::new("test/fixtures/new_has_correct_metadata.wsp", schema).unwrap();
    //     let header = file.header;

    //     let expected_metadata = Metadata {
    //         aggregation_type: AggregationType::Average,
    //         max_retention: 60*60*24*365,
    //         x_files_factor: 0.5,
    //         archive_count: 3
    //     };
    //     assert_eq!(header.metadata, expected_metadata);

    //     let archive_infos = header.archive_infos;
    //     let expected_archive_infos = vec![
    //         // 1m:1h
    //         ArchiveInfo {
    //             offset: 52,
    //             seconds_per_point: 60,
    //             retention: 60*60,
    //             points: 60,
    //         },
    //         // 1h:1w
    //         ArchiveInfo {
    //             offset: 52 + 60*12,
    //             seconds_per_point: 60*60,
    //             retention: 60*60*24*7,
    //             points: 24*7
    //         },
    //         // 1w:1y
    //         ArchiveInfo {
    //             offset: 52 + 60*12 + 24*7*12,
    //             seconds_per_point: 60*60*24*7,
    //             retention: 60*60*24*365,
    //             points: 52
    //         }
    //     ];
    //     assert_eq!(archive_infos.len(), expected_archive_infos.len());
    //     assert_eq!(archive_infos[0], expected_archive_infos[0]);
    //     assert_eq!(archive_infos[1], expected_archive_infos[1]);
    //     assert_eq!(archive_infos[2], expected_archive_infos[2]);
    // }


    // #[test]
    // fn test_split_first_archive() {
    //     let file = open("test/fixtures/60-1440-1440-168-10080-52.wsp").unwrap();
    //     let current_time = time::get_time().sec as u64;
    //     let point_timestamp = current_time - 100;
    //     let (best,rest) = file.split(current_time, point_timestamp).unwrap();

    //     let expected_best = ArchiveInfo {
    //         offset: 52,
    //         seconds_per_point: 60,
    //         points: 1440,
    //         retention: 86400,
    //     };

    //     let expected_rest = vec![
    //         ArchiveInfo {
    //             offset: 17332,
    //             seconds_per_point: 1440,
    //             points: 168,
    //             retention: 241920
    //         },
    //         ArchiveInfo {
    //             offset: 19348,
    //             seconds_per_point: 10080,
    //             points: 52,
    //             retention: 524160
    //         }
    //     ];

    //     // Silly Vec<&T> makes this annoying. See TODO to change to slices.
    //     assert_eq!(rest.len(), 2);
    //     assert_eq!(*(rest[0]), expected_rest[0]);
    //     assert_eq!(*(rest[1]), expected_rest[1]);

    //     assert_eq!(*best, expected_best);
    // }

    // #[test]
    // fn test_split_second_archive() {
    //     let file = open("test/fixtures/60-1440-1440-168-10080-52.wsp").unwrap();
    //     let current_time = time::get_time().sec as u64;

    //     // one sample past the first archive's retention
    //     let point_timestamp = current_time - 60*1441;

    //     let (best,rest) = file.split(current_time, point_timestamp).unwrap();

    //     let expected_best = ArchiveInfo {
    //         offset: 17332,
    //         seconds_per_point: 1440,
    //         points: 168,
    //         retention: 241920
    //     };

    //     let expected_rest = vec![
    //         ArchiveInfo {
    //             offset: 19348,
    //             seconds_per_point: 10080,
    //             points: 52,
    //             retention: 524160
    //         }
    //     ];

    //     // Silly Vec<&T> makes this annoying. See TODO to change to slices.
    //     assert_eq!(rest.len(), 1);
    //     assert_eq!(*(rest[0]), expected_rest[0]);

    //     assert_eq!(*best, expected_best);
    // }

    // #[test]
    // fn test_split_no_archive() {
    //     let file = open("test/fixtures/60-1440-1440-168-10080-52.wsp").unwrap();
    //     let current_time = time::get_time().sec as u64;

    //     // one sample past the first archive's retention
    //     let point_timestamp = current_time - 10080*53;

    //     let split = file.split(current_time, point_timestamp);
    //     assert!(split.is_none());
    // }
}
