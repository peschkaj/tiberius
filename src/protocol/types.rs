use std::borrow::Cow;
use std::io::prelude::*;
use std::io::Cursor;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use chrono::{NaiveDateTime, NaiveDate, NaiveTime, Duration, FixedOffset, TimeZone};
use encoding::{Encoding, DecoderTrap};
use encoding::all::UTF_16LE;
use protocol::WriteTokenStream;
use protocol::util::{FromPrimitive, ReadCharStream, WriteUtf16};
use types::{ColumnValue, ColumnType, Guid};
use super::{DecodeTokenStream};
use ::{TdsResult, TdsError, TdsProtocolError};

#[derive(Debug)]
pub struct Collation {
    // lcid is first 20 bits (12 left), the next 8 bits are copied into flags, the next 4 into version
    lcid: u32,
    /// ignoreCase[1b], ignoreAccent[1b], ignoreKana[1b], ignoreWidth[1b], binary[1b], binary2[1b], reserved[2b]
    flags: u8,
    /// 4 bits!
    version: u8,
    sortid: u8
}

impl DecodeTokenStream for Collation {
    fn decode<T: AsRef<[u8]>>(cursor: &mut Cursor<T>) -> TdsResult<Collation> {
        let mut collation = Collation {
            lcid: try!(cursor.read_u32::<LittleEndian>()),
            sortid: try!(cursor.read_u8()),
            version: 0,
            flags: 0
        };
        collation.flags = (collation.lcid & 0x00000FF0) as u8;
        collation.version = (collation.lcid & 0x0000000F) as u8;
        collation.lcid = collation.lcid & 0xFFFFF000;
        Ok(collation)
    }
}

/// 2.2.5.4.1
#[derive(PartialEq, Debug, Clone)]
#[repr(u8)]
pub enum FixedLenType {
    // NULLTYPE (0x1F) is never emitted from the SQL-server so we do not list it here
    Int1 = 0x30,
    Bit = 0x32,
    Int2 = 0x34,
    Int4 = 0x38,
    /// Small Date Time
    DateTime4 = 0x3A,
    Float4 = 0x3B,
    Money8 = 0x3C,
    DateTime = 0x3D,
    Float8 = 0x3E,
    Money4 = 0x7A,
    Int8 = 0x7F,
}
impl_from_primitive!(FixedLenType, Int1, Bit, Int2, Int4, DateTime4, Float4, Money8, DateTime, Float8, Money4, Int8);

/// 2.2.5.4.2
#[repr(u8)]
#[derive(PartialEq, Debug)]
pub enum VarLenType {
    Guid = 0x24,
    Intn = 0x26,
    Bitn = 0x68,
    Decimaln = 0x6A,
    Numericn = 0x6C,
    Floatn = 0x6D,
    Money = 0x6E,
    Datetimen = 0x6F,
    /// introduced in TDS 7.3
    Daten = 0x28,
    /// introduced in TDS 7.3
    Timen = 0x29,
    /// introduced in TDS 7.3
    Datetime2 = 0x2A,
    /// introduced in TDS 7.3
    DatetimeOffsetn = 0x2B,
    BigVarBin = 0xA5,
    BigVarChar = 0xA7,
    BigBinary = 0xAD,
    BigChar = 0xAF,
    NVarchar = 0xE7,
    NChar = 0xEF,
    // not supported yet
    Xml = 0xF1,
    // not supported yet
    Udt = 0xF0,
    Text = 0x23,
    Image = 0x22,
    NText = 0x63,
    // not supported yet
    SSVariant = 0x62
    // legacy types (not supported since post-7.2):
    // Char = 0x2F,
    // VarChar = 0x27,
    // Binary = 0x2D,
    // VarBinary = 0x25,
    // Numeric = 0x3F,
    // Decimal = 0x37,
}
impl_from_primitive!(VarLenType, Guid, Intn, Bitn, Decimaln, Numericn, Floatn, Money, Datetimen, Daten, Timen, Datetime2, DatetimeOffsetn,
    BigVarBin, BigVarChar, BigBinary, BigChar, NVarchar, NChar, Xml, Udt, Text, Image, NText, SSVariant);

#[derive(Debug)]
pub enum TypeInfo {
    FixedLenType(FixedLenType),
    /// VARLENTYPE TYPE_VARLEN [COLLATION]
    VarLenType(VarLenType, u32, Option<Collation>),
    /// VARLENTYPE TYPE_VARLEN [PRECISION SCALE]
    VarLenTypeP(VarLenType, u32, u8, u8),
    /// VARLENTYPE SCALE (>=TDS 7.3)
    VarLenTypeS(VarLenType, u8)
}

impl DecodeTokenStream for TypeInfo {
    fn decode<T: AsRef<[u8]>>(cursor: &mut Cursor<T>) -> TdsResult<TypeInfo> {
        let tyid = try!(cursor.read_u8());
        let tmp = FromPrimitive::from(tyid);

        Ok(match tmp {
            None => {
                let tmp: Option<VarLenType> = FromPrimitive::from(tyid);
                match tmp {
                    None => return Err(TdsError::Other(format!("column data type {} not supported", tyid))),
                    Some(var_len_type) => {
                        let mut has_collation = false;
                        let mut has_precision = false;
                        let mut has_scale = false;

                        let len = match var_len_type {
                            VarLenType::Guid | VarLenType::Intn | VarLenType::Datetimen | VarLenType::Floatn | VarLenType::Money | VarLenType::Bitn => try!(cursor.read_u8()) as u32,
                            VarLenType::NVarchar | VarLenType::BigChar | VarLenType::BigVarChar | VarLenType::NChar => {
                                // TODO: BIGCHARTYPE also include collation
                                has_collation = true;
                                try!(cursor.read_u16::<LittleEndian>()) as u32
                            },
                            VarLenType::BigBinary | VarLenType::BigVarBin => {
                                try!(cursor.read_u16::<LittleEndian>()) as u32
                            },
                            VarLenType::Text | VarLenType::NText => {
                                has_collation = true;
                                try!(cursor.read_i32::<LittleEndian>()) as u32
                            },
                            VarLenType::Image => try!(cursor.read_u32::<LittleEndian>()),
                            VarLenType::Decimaln | VarLenType::Numericn => {
                                has_precision = true;
                                try!(cursor.read_u8()) as u32
                            },
                            VarLenType::Datetime2 | VarLenType::Timen | VarLenType::DatetimeOffsetn => {
                                has_scale = true;
                                0
                            },
                            VarLenType::Daten => 0,
                            _ => return Err(TdsError::Other(format!("variable length type {:?} not supported", var_len_type)))
                        };
                        match true {
                            true if has_collation => TypeInfo::VarLenType(var_len_type, len, Some(try!(Collation::decode(cursor)))),
                            true if has_precision => TypeInfo::VarLenTypeP(var_len_type, len, try!(cursor.read_u8()), try!(cursor.read_u8())),
                            true if has_scale => TypeInfo::VarLenTypeS(var_len_type, try!(cursor.read_u8())),
                            _ => TypeInfo::VarLenType(var_len_type, len, None)
                        }
                    }
                }
            },
            Some(x) => TypeInfo::FixedLenType(x)
        })
    }
}

/// 2.2.7.4
#[derive(Debug)]
pub struct ColumnData {
    pub user_type: u32,
    /// fNullable[1b], fCaseSen[1b], usUpdateable[2b], fIdentity[1b], fComputed[1b], usReservedODBC[2b]
    /// fFixedLenCLRType[1b], usReserved[4b], fHidden[1b], fKey[1b], fNullableUnknown[1b]
    pub flags: u16,
    pub type_info: TypeInfo,
    pub table_name: Option<Vec<String>>,
    pub col_name: Option<String>,
}

impl DecodeTokenStream for ColumnData {
    fn decode<T: AsRef<[u8]>>(cursor: &mut Cursor<T>) -> TdsResult<ColumnData> {
        let user_type = try!(cursor.read_u32::<LittleEndian>());
        let flags = try!(cursor.read_u16::<LittleEndian>());

        let type_info = try!(TypeInfo::decode(cursor));
        let has_tablename = match type_info {
            TypeInfo::VarLenType(ref ty, _, _) | TypeInfo::VarLenTypeP(ref ty, _, _, _) => {
                match *ty {
                    VarLenType::Text | VarLenType::NText | VarLenType::Image => true,
                    _ => false
                }
            },
            _ => false
        };
        let tablename = if has_tablename {
            let parts = try!(cursor.read_u8());
            match parts {
                0 => None,
                _ => {
                    let mut data: Vec<String> = Vec::with_capacity(parts as usize);
                    for _ in 0..parts {
                        data.push(try!(cursor.read_us_varchar()));
                    }
                    Some(data)
                }
            }
        } else {
            None
        };

        // colname
        let colname = try!(cursor.read_b_varchar());
        Ok(ColumnData {
            user_type: user_type,
            flags: flags,
            type_info: type_info,
            table_name: tablename,
            col_name: Some(colname)
        })
    }
}

impl ColumnData {
    #[inline]
    pub fn is_nullable(&self) -> bool {
        (self.flags & 1) == 1
    }
}

impl<'a, W: Write> WriteTokenStream<&'a ColumnType<'a>> for W {
    fn write_token_stream(&mut self, data: &'a ColumnType<'a>) -> TdsResult<()> {
        match *data {
            ColumnType::I8(ref val) => {
                try!(self.write_u8(VarLenType::Intn as u8));
                try!(self.write_u8(1));
                try!(self.write_u8(1));
                try!(self.write_i8(*val))
            },
            ColumnType::I16(ref val) => {
                try!(self.write_u8(VarLenType::Intn as u8));
                try!(self.write_u8(2));
                try!(self.write_u8(2));
                try!(self.write_i16::<LittleEndian>(*val));
            },
            ColumnType::I32(ref val) => {
                try!(self.write_u8(VarLenType::Intn as u8));
                try!(self.write_u8(4));
                try!(self.write_u8(4));
                try!(self.write_i32::<LittleEndian>(*val));
            },
            ColumnType::I64(ref val) => {
                try!(self.write_u8(VarLenType::Intn as u8));
                try!(self.write_u8(8));
                try!(self.write_u8(8));
                try!(self.write_i64::<LittleEndian>(*val));
            },
            ColumnType::F32(ref val) => {
                try!(self.write_u8(VarLenType::Floatn as u8));
                try!(self.write_u8(4));
                try!(self.write_u8(4));
                try!(self.write_f32::<LittleEndian>(*val));
            },
            ColumnType::F64(ref val) => {
                try!(self.write_u8(VarLenType::Floatn as u8));
                try!(self.write_u8(8));
                try!(self.write_u8(8));
                try!(self.write_f64::<LittleEndian>(*val));
            },
            ColumnType::String(ref val) => {
                let len = (val.len() as u32 * 2) as u16;
                try!(self.write_u8(VarLenType::NVarchar as u8));
                try!(self.write_u16::<LittleEndian>(len));
                try!(self.write_all(&[0, 0, 0, 0, 0])); //todo use a non-hardcoded collation
                try!(self.write_u16::<LittleEndian>(len));
                try!(self.write_as_utf16(&val));
            },
            _ => panic!("rpc: encoding of ColumnType {:?} not supported", data)
        }
        Ok(())
    }
}

#[inline]
fn decode_datetime<T: AsRef<[u8]>>(ty: FixedLenType, cursor: &mut Cursor<T>) -> TdsResult<NaiveDateTime> {
    let days: i64;
    let duration = match ty {
        FixedLenType::DateTime4 => {
            // days since 1.1.1900
            days = try!(cursor.read_u16::<LittleEndian>()) as i64;
            // number of minutes since 12:00 (AM)
            let mins = try!(cursor.read_u16::<LittleEndian>());
            Duration::minutes(mins as i64)
        },
        FixedLenType::DateTime => {
            // days since 1.1.1900
            days = try!(cursor.read_u32::<LittleEndian>()) as i64;
            // number of 1/300 since 12am
            let ticks = try!(cursor.read_u32::<LittleEndian>());
            Duration::nanoseconds((1E9/300f64 * ticks as f64) as i64)
        },
        _ => unreachable!()
    };
    let date = NaiveDate::from_ymd(1900, 1, 1) + Duration::days(days as i64);
    Ok(date.and_hms(0, 0, 0) + duration)
}

/// decode a TDS 7.3 date
#[inline]
fn decode_date<T: AsRef<[u8]>>(cursor: &mut Cursor<T>) -> TdsResult<NaiveDate> {
    // number of days since January 1, year 1
    let days = try!(cursor.read_u16::<LittleEndian>()) as u32 | (try!(cursor.read_u8()) as u32) << 16;
    Ok(NaiveDate::from_ymd(1, 1, 1) + Duration::days(days as i64))
}

/// decode a TDS 7.3 time
fn decode_time<T: AsRef<[u8]>>(scale: u8, cursor: &mut Cursor<T>) -> TdsResult<NaiveTime> {
    // 10^-n second increments since 12 AM
    let increments = match scale {
        0...2 => try!(cursor.read_u16::<LittleEndian>()) as u64 | (try!(cursor.read_u8()) as u64) << 16,
        3...4 => try!(cursor.read_u32::<LittleEndian>()) as u64,
        5...7 => try!(cursor.read_u32::<LittleEndian>()) as u64 | (try!(cursor.read_u8()) as u64) << 32,
        _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("datetime2: scale of {} is invalid", scale))))
    };
    let duration = Duration::nanoseconds((increments as f64/(10u64.pow(scale as u32) as f64)*1e9f64) as i64);
    Ok(NaiveTime::from_hms(0, 0, 0) + duration)
}

#[inline]
fn decode_money<'a, T: AsRef<[u8]>>(ty: FixedLenType, cursor: &mut Cursor<T>) -> TdsResult<ColumnType<'a>> {
    Ok(match ty {
        FixedLenType::Money4 => ColumnType::F32(try!(cursor.read_i32::<LittleEndian>()) as f32 / (10u32.pow(4) as f32)),
        FixedLenType::Money8 => {
            let mut val: i64 = (try!(cursor.read_i32::<LittleEndian>()) as i64) << 32;
            val |= try!(cursor.read_i32::<LittleEndian>()) as i64;
            ColumnType::F64(val as f64 / (10u32.pow(4) as f64))
        },
        _ => unreachable!()
    })
}

/// basically decodes a `TYPE_VARBYTE`
impl<'a> ColumnValue<'a> {
    pub fn decode<T: AsRef<[u8]>>(cursor: &mut Cursor<T>, tyinfo: &TypeInfo) -> TdsResult<ColumnValue<'a>> {
        Ok(match *tyinfo {
            TypeInfo::FixedLenType(ref f_type) => {
                match *f_type {
                    FixedLenType::Bit => ColumnValue::Some(ColumnType::Bool(try!(cursor.read_u8()) == 1)),
                    FixedLenType::Int1 => ColumnValue::Some(ColumnType::I8(try!(cursor.read_i8()))),
                    FixedLenType::Int2 => ColumnValue::Some(ColumnType::I16(try!(cursor.read_i16::<LittleEndian>()))),
                    FixedLenType::Int4 => ColumnValue::Some(ColumnType::I32(try!(cursor.read_i32::<LittleEndian>()))),
                    FixedLenType::Int8 => ColumnValue::Some(ColumnType::I64(try!(cursor.read_i64::<LittleEndian>()))),
                    FixedLenType::Float4 => ColumnValue::Some(ColumnType::F32(try!(cursor.read_f32::<LittleEndian>()))),
                    FixedLenType::Money4 => ColumnValue::Some(try!(decode_money(FixedLenType::Money4, cursor))),
                    FixedLenType::Float8 => ColumnValue::Some(ColumnType::F64(try!(cursor.read_f64::<LittleEndian>()))),
                    FixedLenType::Money8 => ColumnValue::Some(try!(decode_money(FixedLenType::Money8, cursor))),
                    FixedLenType::DateTime4 | FixedLenType::DateTime => {
                        ColumnValue::Some(ColumnType::Datetime(try!(decode_datetime(f_type.clone(), cursor))))
                    }
                }
            },
            TypeInfo::VarLenType(ref v_type, _, ref collation) => {
                match *v_type {
                    VarLenType::BigChar | VarLenType::BigVarChar => {
                        let len = try!(cursor.read_u16::<LittleEndian>());
                        if len == 0xFFFF {
                            ColumnValue::None
                        } else {
                            let mut buf = vec![0; len as usize];
                            try!(cursor.read(&mut buf));
                            match String::from_utf8(buf) {
                                Err(x) => return Err(TdsError::Conversion(Box::new(x))),
                                Ok(x) => ColumnValue::Some(ColumnType::String(Cow::Owned(x)))
                            }
                        }
                    },
                    VarLenType::NVarchar | VarLenType::NChar => {
                        let len = try!(cursor.read_u16::<LittleEndian>());
                        if len == 0xFFFF {
                            ColumnValue::None
                        } else {
                            let mut buf = vec![0; len as usize];
                            try!(cursor.read(&mut buf));
                            ColumnValue::Some(ColumnType::String(Cow::Owned(try!(UTF_16LE.decode(&buf, DecoderTrap::Strict)))))
                        }
                    },
                    VarLenType::BigBinary | VarLenType::BigVarBin => {
                        let len = try!(cursor.read_u16::<LittleEndian>());
                        if len == 0xFFFF {
                            ColumnValue::None
                        } else {
                            let mut buf = vec![0; len as usize];
                            try!(cursor.read(&mut buf));
                            ColumnValue::Some(ColumnType::Binary(buf))
                        }
                    },
                    VarLenType::Text | VarLenType::NText | VarLenType::Image => {
                        // TODO what is textptr dummy stuff...
                        match try!(cursor.read_u8()) {
                            0 => ColumnValue::None,
                            text_ptr_len => {
                                let mut buf = vec![0; text_ptr_len as usize]; //text_ptr
                                try!(cursor.read(&mut buf));
                                // Timestamp TODO: what is this..
                                let mut timestamp = [0; 8];
                                try!(cursor.read(&mut timestamp));
                                let len = try!(cursor.read_i32::<LittleEndian>());

                                if len < -1 {
                                    return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("text: invalid length of {}", len))));
                                }
                                if len < 0 {
                                    ColumnValue::None
                                } else {
                                    let mut buf = vec![0; len as usize];
                                    try!(cursor.read(&mut buf));
                                    match *v_type {
                                        VarLenType::Text => match String::from_utf8(buf) {
                                            Err(x) => return Err(TdsError::Conversion(Box::new(x))),
                                            Ok(x) => ColumnValue::Some(ColumnType::String(Cow::Owned(x)))
                                        },
                                        VarLenType::NText => {
                                            ColumnValue::Some(ColumnType::String(Cow::Owned(try!(UTF_16LE.decode(&buf, DecoderTrap::Strict)))))
                                        },
                                        VarLenType::Image => {
                                            ColumnValue::Some(ColumnType::Binary(buf))
                                        }
                                        _ => unreachable!(),
                                    }
                                }
                            }
                        }
                    },
                    VarLenType::Intn => {
                        let len = try!(cursor.read_u8());
                        match len {
                            0 => ColumnValue::None,
                            1 => ColumnValue::Some(ColumnType::I8(try!(cursor.read_i8()))),
                            2 => ColumnValue::Some(ColumnType::I16(try!(cursor.read_i16::<LittleEndian>()))),
                            4 => ColumnValue::Some(ColumnType::I32(try!(cursor.read_i32::<LittleEndian>()))),
                            8 => ColumnValue::Some(ColumnType::I64(try!(cursor.read_i64::<LittleEndian>()))),
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("intn: length of {} is invalid", len))))
                        }
                    },
                    VarLenType::Guid => {
                        let len = try!(cursor.read_u8());
                        match len {
                            0x10 => ColumnValue::Some(ColumnType::Guid(try!(Guid::decode(cursor)))),
                            0x00 => ColumnValue::None,
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("guid: length of {} is invalid", len))))
                        }
                    },
                    VarLenType::Datetimen => {
                        let len = try!(cursor.read_u8());
                        match len {
                            0 => ColumnValue::None,
                            4 => ColumnValue::Some(ColumnType::Datetime(try!(decode_datetime(FixedLenType::DateTime4, cursor)))),
                            8 => ColumnValue::Some(ColumnType::Datetime(try!(decode_datetime(FixedLenType::DateTime, cursor)))),
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("datetimen: length of {} is invalid", len))))
                        }
                    },
                    /// 2.2.5.5.1.5 IEEE754
                    VarLenType::Floatn => {
                        let len = try!(cursor.read_u8());
                        match len {
                            0 => ColumnValue::None,
                            4 => ColumnValue::Some(ColumnType::F32(try!(cursor.read_f32::<LittleEndian>()))),
                            8 => ColumnValue::Some(ColumnType::F64(try!(cursor.read_f64::<LittleEndian>()))),
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("floatn: length of {} is invalid", len))))
                        }
                    },
                    VarLenType::Money => {
                        let len = try!(cursor.read_u8());
                        match len {
                            0 => ColumnValue::None,
                            4 => ColumnValue::Some(try!(decode_money(FixedLenType::Money4, cursor))),
                            8 => ColumnValue::Some(try!(decode_money(FixedLenType::Money8, cursor))),
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("money: length of {} is invalid", len))))
                        }
                    },
                    VarLenType::Bitn => {
                        let len = try!(cursor.read_u8());
                        match len {
                            0 => ColumnValue::None,
                            1 => ColumnValue::Some(ColumnType::Bool(try!(cursor.read_u8()) != 0)),
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("bitn: length of {} is invalid", len))))
                        }
                    },
                    VarLenType::Daten => {
                        let len = try!(cursor.read_u8());
                        match len {
                            0 => ColumnValue::None,
                            3 => ColumnValue::Some(ColumnType::Date(try!(decode_date(cursor)))),
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("timen: length of {} is invalid", len))))
                        }
                    },
                    _ => panic!("unsupported vtype {:?}", v_type)
                }
            },
            TypeInfo::VarLenTypeP(ref v_type, _, ref precision, ref scale) => {
                match *v_type {
                    VarLenType::Decimaln | VarLenType::Numericn => {
                        let len = try!(cursor.read_u8());
                        let sign = try!(cursor.read_u8()) == 0;
                        let f = if sign { -1.0 } else { 1.0 };

                        match len {
                            5 => ColumnValue::Some(ColumnType::F64(f * try!(cursor.read_u32::<LittleEndian>()) as f64 / (10f64).powi(*scale as i32))),
                            9 => ColumnValue::Some(ColumnType::F64(f * try!(cursor.read_u64::<LittleEndian>()) as f64 / (10f64).powi(*scale as i32))),
                            _ => return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("decimal: length of {} is unsupported", *precision))))
                        }
                    },
                    _ => panic!("unsupported scaled vtype {:?}", v_type)
                }
            },
            TypeInfo::VarLenTypeS(ref v_type, ref scale) => {
                match *v_type {
                    VarLenType::Datetime2 => {
                        let len = try!(cursor.read_u8());

                        if len == 0 {
                            ColumnValue::None
                        } else if (len == 6 && *scale < 3) || (len == 7 && *scale < 5) || (len == 8 && *scale < 8) {
                            let time = try!(decode_time(*scale, cursor));
                            let date = try!(decode_date(cursor));
                            let datetime = NaiveDateTime::new(date, time);
                            ColumnValue::Some(ColumnType::Datetime(datetime))
                        } else {
                            return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("datetime2: length of {} with scale {} is unsupported", len, scale))));
                        }
                    },
                    VarLenType::Timen => {
                        let len = try!(cursor.read_u8());
                        if len == 0 {
                            ColumnValue::None
                        } else if (len == 3 && *scale < 3) || (len == 4 && *scale < 5) || (len == 5 && *scale < 8) {
                            let time = try!(decode_time(*scale, cursor));
                            ColumnValue::Some(ColumnType::Time(time))
                        } else {
                            return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("timen: length of {} with scale {} is unsupported", len, scale))));
                        }
                    },
                    VarLenType::DatetimeOffsetn => {
                        let len = try!(cursor.read_u8());
                        if len == 0 {
                            ColumnValue::None
                        } else if (len == 8 && *scale < 3) || (len == 9 && *scale < 5) || (len == 10 && *scale < 8) {
                            let time = try!(decode_time(*scale, cursor));
                            let date = try!(decode_date(cursor));
                            let datetime = NaiveDateTime::new(date, time);
                            // number of minutes from UTC
                            let offset = try!(cursor.read_i16::<LittleEndian>());
                            ColumnValue::Some(ColumnType::Datetime(FixedOffset::east(offset as i32 * 60).from_utc_datetime(&datetime).naive_utc()))
                        } else {
                            return Err(TdsError::ProtocolError(TdsProtocolError::InvalidLength(format!("datetimeoffset: length of {} with scale {} is unsupported", len, scale))));
                        }
                    },
                    _ => panic!("unsupported scale-only vtype {:?}", v_type)
                }
            },
        })
    }
}
