use ihex::reader::{
    Reader,
    ReaderError,
};
use ihex::record::Record::{
    self,
    *,
};
use scroll::Pread;
use coresight::access_ports::AccessPortError;
use std::time::Instant;
use log::info;
use std::thread;
use std::time;
use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub enum FlashError {
    ReaderError(ReaderError),
    AccessPortError(AccessPortError),
}

impl fmt::Display for FlashError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use FlashError::*;

        match self {
            ReaderError(ref e) => e.fmt(f),
            AccessPortError(ref e) => e.fmt(f),
        }
    }

}

impl Error for FlashError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        use FlashError::*;

        match self {
            ReaderError(ref e) => Some(e),
            AccessPortError(ref e) => Some(e),
        }
    }
}

impl From<AccessPortError> for FlashError {
    fn from(e: AccessPortError) -> Self {
        FlashError::AccessPortError(e)
    }
}

impl From<ReaderError> for FlashError {
    fn from(e: ReaderError) -> Self {
        FlashError::ReaderError(e)
    }
}

pub fn download_hex<P: super::MI, S: Into<String>>(file_path: S, probe: &mut P, page_size: u32) -> Result<(), FlashError> {
    let mut extended_segment_address = 0;
    let mut extended_linear_address = 0;

    let mut total_bytes = 0;

    // Start timer.
    let instant = Instant::now();

    let hex_file = std::fs::read_to_string(file_path.into()).unwrap();
    let hex = Reader::new(&hex_file);

    for record in hex {
        let record = record?;
        match record {
            Data {
                offset: offset,
                value: value,
            } => {
                let offset = extended_linear_address | offset as u32;
                if offset % page_size == 0 {
                    erase_page(probe, offset)?;
                }
                write_bytes(probe, offset, value.as_slice())?;
                total_bytes += value.len();
                // Stop timer.
                let elapsed = instant.elapsed();
                println!("Wrote {} total 32bit words in {:.2?} seconds. Current addr: {}", total_bytes, elapsed, offset);
            },
            EndOfFile => return Ok(()),
            ExtendedSegmentAddress(address) => { extended_segment_address = address * 16; },
            StartSegmentAddress { .. } => (),
            ExtendedLinearAddress(address) => { extended_linear_address = (address as u32) << 16; },
            StartLinearAddress(_) => (),
        };
    }

    Ok(())
}

fn write_bytes<P: super::MI>(probe: &mut P, address: u32, data: &[u8]) -> Result<(), AccessPortError> {
    let NVMC = 0x4001E000;
    let NVMC_CONFIG = NVMC + 0x504;
    let WEN: u32 = 0x1;

    info!("Writing to address 0x{:08x}", address);

    probe.write(NVMC_CONFIG, WEN)?;
    probe.write_block(
        address,
        data.chunks(4)
            .map(|c|
                c.pread::<u32>(0)
                 .expect("This is a bug. Please report it.")
            )
            .collect::<Vec<_>>()
            .as_slice()
        )
}

fn erase_page<P: super::MI>(probe: &mut P, address: u32) -> Result<(), AccessPortError> {
    let NVMC = 0x4001E000;
    let NVMC_READY = 0x4001E000 + 0x400;
    let NVMC_CONFIG = NVMC + 0x504;
    let NVMC_ERASEPAGE = NVMC + 0x508;
    let EEN: u32 = 0x2;

    info!("Erasing page {:04} (0x{:08x})",address / 1024, address);

    probe.write(NVMC_CONFIG, EEN)?;
    probe.write(NVMC_ERASEPAGE, address)?;

    let mut read_flag: u32 = probe.read(NVMC_READY)?;

    while read_flag == 0 {
        info!("NVM busy (flag=0x{:08x}), waiting...", read_flag);
        read_flag = probe.read(NVMC_READY)?;
        thread::sleep(time::Duration::from_millis(1));
    }

    Ok(())
}