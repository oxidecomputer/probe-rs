use super::super::{Category, CmsisDapError, Request, Response, Result, Status};
use anyhow::anyhow;
use scroll::{Pread, Pwrite, LE};

#[allow(unused)]
#[derive(Copy, Clone, Debug)]
pub enum Transport {
    None = 0,
    TransportDataViaSWO = 1,
    TransportDataViaWinUSB = 2,
}

impl Request for Transport {
    const CATEGORY: Category = Category(0x17);

    fn to_bytes(&self, buffer: &mut [u8], offset: usize) -> Result<usize> {
        buffer[offset] = *self as u8;
        Ok(1)
    }
}

#[derive(Debug)]
pub struct TransportResponse(pub(crate) Status);

impl Response for TransportResponse {
    fn from_bytes(buffer: &[u8], offset: usize) -> Result<Self> {
        Ok(TransportResponse(Status::from_byte(buffer[offset])?))
    }
}

#[allow(unused)]
#[derive(Copy, Clone, Debug)]
pub enum Mode {
    Off = 0,
    UART = 1,
    Manchester = 2,
}

impl Request for Mode {
    const CATEGORY: Category = Category(0x18);

    fn to_bytes(&self, buffer: &mut [u8], offset: usize) -> Result<usize> {
        buffer[offset] = *self as u8;
        Ok(1)
    }
}

#[derive(Debug)]
pub struct ModeResponse(pub(crate) Status);

impl Response for ModeResponse {
    fn from_bytes(buffer: &[u8], offset: usize) -> Result<Self> {
        Ok(ModeResponse(Status::from_byte(buffer[offset])?))
    }
}

#[derive(Copy, Clone, Debug)]
pub struct BaudRate(pub(crate) u32);

impl Request for BaudRate {
    const CATEGORY: Category = Category(0x19);

    fn to_bytes(&self, buffer: &mut [u8], offset: usize) -> Result<usize> {
        buffer
            .pwrite_with::<u32>(self.0, offset, LE)
            .expect("This is a bug. Please report it.");

        Ok(4)
    }
}

impl Response for BaudRate {
    fn from_bytes(buffer: &[u8], offset: usize) -> Result<Self> {
        let res = buffer
            .pread_with::<u32>(offset, LE)
            .expect("This is a bug. Please report it.");
        Ok(BaudRate(res))
    }
}

#[allow(unused)]
#[derive(Copy, Clone, Debug)]
pub enum Control {
    Stop = 0,
    Start = 1,
}

impl Request for Control {
    const CATEGORY: Category = Category(0x1a);

    fn to_bytes(&self, buffer: &mut [u8], offset: usize) -> Result<usize> {
        buffer[offset] = *self as u8;
        Ok(1)
    }
}

#[derive(Debug)]
pub struct ControlResponse(pub(crate) Status);

impl Response for ControlResponse {
    fn from_bytes(buffer: &[u8], offset: usize) -> Result<Self> {
        Ok(ControlResponse(Status::from_byte(buffer[offset])?))
    }
}

#[derive(Debug)]
pub struct StatusRequest;

impl Request for StatusRequest {
    const CATEGORY: Category = Category(0x1b);

    fn to_bytes(&self, _buffer: &mut [u8], _offset: usize) -> Result<usize> {
        Ok(0)
    }
}

#[derive(Copy, Clone, Debug)]
pub struct TraceStatus {
    pub active: bool,
    pub error: bool,
    pub overrun: bool,
}

impl From<u8> for TraceStatus {
    fn from(value: u8) -> Self {
        Self {
            active: value & (1 << 0) != 0,
            error: value & (1 << 6) != 0,
            overrun: value & (1 << 7) != 0,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct StatusResponse {
    pub status: TraceStatus,
    pub count: u32,
}

impl Response for StatusResponse {
    fn from_bytes(buffer: &[u8], offset: usize) -> Result<Self> {
        let status = TraceStatus::from(buffer[offset]);

        let count = buffer
            .pread_with::<u32>(offset + 1, LE)
            .expect("This is a bug. Please report it.");

        Ok(StatusResponse {
            status: status,
            count: count,
        })
    }
}

#[derive(Debug)]
pub struct DataRequest(pub u32);

impl Request for DataRequest {
    const CATEGORY: Category = Category(0x1c);

    fn to_bytes(&self, buffer: &mut [u8], offset: usize) -> Result<usize> {
        let size: u16 = self.0 as u16;

        buffer
            .pwrite_with::<u16>(size, offset, LE)
            .expect("This is a bug. Please report it.");

        Ok(2)
    }
}

#[derive(Debug)]
pub struct DataResponse {
    pub status: TraceStatus,
    pub data: Vec<u8>,
}

impl Response for DataResponse {
    fn from_bytes(buffer: &[u8], offset: usize) -> Result<Self> {
        let status = TraceStatus::from(buffer[offset]);

        let count = buffer
            .pread_with::<u16>(offset + 1, LE)
            .expect("This is a bug. Please report it.");

        let start = offset + 3;
        let end = start + count as usize;

        if end > buffer.len() {
            log::debug!(
                "short read (or bad count): expected {} bytes, found {}",
                count,
                buffer.len() - start
            );
            Err(anyhow!(CmsisDapError::ShortRead))
        } else {
            Ok(DataResponse {
                status: status,
                data: buffer[start..end].to_vec(),
            })
        }
    }
}
