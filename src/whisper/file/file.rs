// use std::io::Error;
use std::fs::File;
use std::io::{BufWriter, SeekFrom, Cursor, Seek, Read};
use byteorder::{ByteOrder, BigEndian, WriteBytesExt, ReadBytesExt};
use std::path::Path;
use std::fmt;

use super::header::{ Header, read_header };
use super::write_op::{ WriteOp };

use super::archive_info::{ ArchiveInfo };
use whisper::point;

pub struct WhisperFile<'a> {
    pub path: &'a str,
    pub handle: File,
    pub header: Header
}

impl<'a> fmt::Debug for WhisperFile<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "path:  {:?}, header: {:?}", self.path, self.header)
    }
}

// TODO: Change error value to generic Error
pub fn open(path: &str) -> Result<WhisperFile, &'static str> {
    debug!("opening file");
    let file_handle = File::open(Path::new(path));

    match file_handle {
        Ok(f) => {
            let header = try!(read_header(&f));
            let whisper_file = WhisperFile { path: path, header: header, handle: f };
            Ok( whisper_file )
        },
        Err(_) => {
            Err("generic file error")
        }
    }
}

impl<'a> WhisperFile<'a> {
    // The header information is not thread safe,
    // other processes may open the file and modify the contents
    // which would be bad. It's your job, the caller, to make sure
    // the system can't do that.
    pub fn write(&mut self, current_time: u64, point: point::Point) -> Vec<WriteOp>{ 
        let pair = {
            self.split_archives(current_time, point.timestamp)
        };

        match pair {
            Some( (high_precision_archive, rest) ) => {
                let base_point = {
                    let f = &mut self.handle;
                    read_point(f, high_precision_archive.offset)
                };
                let base_timestamp = base_point.timestamp;
                self.calculate_write_ops( (high_precision_archive, rest) , point, base_timestamp)
            },
            None => {
                panic!("no archives satisfy current time")
            }
        }
    }

    fn split_archives(&self, current_time: u64, point_timestamp: u64) -> Option<(&ArchiveInfo, Vec<&ArchiveInfo>)>  {
        let mut archive_iter = self.header.archive_infos.iter();
        
        let hp_ai_option = archive_iter.find(|ai|
            (current_time - point_timestamp) < ai.retention
        );

        match hp_ai_option {
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

    pub fn calculate_write_ops(&self, (ai,_): (&ArchiveInfo, Vec<&ArchiveInfo>), point: point::Point, base_timestamp: u64) -> Vec<WriteOp> {
        let mut write_ops = vec![];

        {
            let write_op = build_write_op( ai, point, base_timestamp );
            write_ops.push( write_op );
        }


        write_ops
    }
}

fn read_point(file: &mut File, offset: u64) -> point::Point {
    let seek = file.seek(SeekFrom::Start(offset));

    match seek {
        Ok(_) => {
            let mut points_buf : [u8; 12] = [0; 12];
            let mut buf_ref : &mut [u8] = &mut points_buf;
            let read = file.read(buf_ref);

            match read {
                Ok(_) => {
                    let mut cursor = Cursor::new(buf_ref);
                    let timestamp = cursor.read_u32::<BigEndian>().unwrap() as u64;
                    let value = cursor.read_f64::<BigEndian>().unwrap();

                    point::Point{ timestamp: timestamp, value: value }
                },
                Err(err) => {
                    panic!("read point {:?}", err)
                }
            }
        },
        Err(err) => {
            panic!("read_point {:?}", err)
        }
    }
}

fn build_write_op(archive_info: &ArchiveInfo, point: point::Point, base_timestamp: u64) -> WriteOp {
    let mut output_data = [0; 12];
    let interval_ceiling = archive_info.interval_ceiling(&point);

    let point_value = point.value;
    {
        let buf : &mut [u8] = &mut output_data;
        let mut writer = BufWriter::new(buf);
        writer.write_u32::<BigEndian>(interval_ceiling as u32).unwrap();
        writer.write_f64::<BigEndian>(point_value).unwrap();
    }

    let seek_info = archive_info.calculate_seek(&point,  base_timestamp);

    return WriteOp {
        seek: seek_info,
        bytes: output_data
    }
}


#[test]
fn has_write_ops(){
    let whisper_file = WhisperFile{
        path: "/a/nonsense/path",
        header: Header {
            metadata: Metadata {
                aggregation_type: AggregationType::Average,
                max_retention: 86400,
                x_files_factor: 1056964608,
                archive_count: 1
            },
            archive_infos: vec![
                ArchiveInfo {
                    offset: 28,
                    seconds_per_point: 60,
                    points: 1440,
                    retention: 60 * 1440
                },
                ArchiveInfo {
                    offset: 56,
                    seconds_per_point: 60,
                    points: 1440,
                    retention: 60 * 1440
                }
            ]
        }
    };

    let fixture_time = 20;
    let write_ops = whisper_file.calculate_write_ops(
        fixture_time,
        point::Point{value: 0.0, timestamp: 10}
    );

    let expected = vec![
        WriteOp{offset: 28, value: 0.0},
        WriteOp{offset: 56, value: 0.0}
    ];
    assert_eq!(write_ops, expected);

    return;
}
