use std::borrow::Cow;
use std::io::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use types::ColumnType;
use protocol::util::WriteCharStream;
use ::TdsResult;

#[repr(u8)]
#[derive(Clone, Debug)]
pub enum RpcProcId {
    SpExecuteSql = 10,
    SpPrepare = 11,
    SpExecute = 12,
    SpUnprepare = 15,
}

// only used for cipher stuff in TDS >= 7.4, which we dont support yet
// struct RpcParamCipher {
//     type_info: TypeInfo
// }

/// pass the parameter by reference (e.g. OUTPUT)
pub const fByRefValue: u8 = 0x01;

pub const fWithRecomp: u16 = 0x01;
/// client already cached meta data
pub const fNoMetaData: u16 = 0x02;
pub const fReuseMetaData: u16 = 0x04;

#[derive(Debug)]
pub struct RpcParamData<'a> {
    pub name: Cow<'a, str>,
    // fByRefValue[1b], fDefaultValue[1b], reserved[1b], fEncrypted[1b], reserved[4b]
    pub status_flags: u8,
    pub value: ColumnType<'a>,
}

#[derive(Debug)]
pub enum RpcProcIdValue<'a> {
    Name(Cow<'a, str>),
    Id(RpcProcId)
}

pub trait WriteRpcProcId {
    fn write_rpc_procid(&mut self, proc_id: &RpcProcIdValue) -> TdsResult<()>;
}

impl<W: Write> WriteRpcProcId for W {
    fn write_rpc_procid(&mut self, proc_id: &RpcProcIdValue) -> TdsResult<()> {
        match *proc_id {
            RpcProcIdValue::Name(ref name) => try!(self.write_us_varchar(name)),
            RpcProcIdValue::Id(ref id) => {
                try!(self.write_u16::<LittleEndian>(0xFFFF));
                try!(self.write_u16::<LittleEndian>((*id).clone() as u16))
            }
        }
        Ok(())
    }
}

/// 2.2.6.6
#[derive(Debug)]
pub struct RpcRequestData<'a> {
    // NameLenProcID: US_VARCHAR || (0xFFFF USHORT(ProcId))
    pub proc_id: RpcProcIdValue<'a>,
    // fWithRecomp[1b], fNoMetaData[1b], fReuseMetaData[1b], reserved[5b]
    pub flags: u16,
    // reserved[8b]
    pub params: Vec<RpcParamData<'a>>
}
