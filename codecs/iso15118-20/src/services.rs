//! ServiceDiscovery, ServiceDetail and ServiceSelection (`V2G_CI_CommonMessages`).
//!
//! Event-code rules, all checked against EXIficient vectors:
//! - `maxOccurs` lists are unrolled: the first `SE` is a 1-bit code; after each entry
//!   below the maximum a 2-bit code follows (0 = another entry, 1 = end of the list); after
//!   the last allowed entry only the end remains (1 bit).
//! - `Parameter` has a required attribute `Name` (1-bit `AT` code), then a choice of
//!   six value elements (3-bit code).
//! - `xs:byte` is an 8-bit offset, `xs:short` and `xs:int` are EXI integers.
//! - Attribute and string values go through the EXI string table: a repeated value is a
//!   hit (`uint 0` and a local index, or `uint 1` and a global index). Only literals are
//!   added to the table.
//!
//! Service ids are listed in ISO 15118-20:2022 Table 204 (via EcoG, unchecked).

use exi::Arena;
use exi::bits::{BitReader, BitWriter};
use crate::common::*;
use exi::{DecodeError, EncodeError};

pub const MAX_SERVICE_IDS: usize = 16;
pub const MAX_SERVICES: usize = 8;
pub const MAX_SELECTED_SERVICES: usize = 16;
pub const MAX_PARAMETERS: usize = 32;
pub const MAX_PARAMETER_SETS: usize = 32;
pub const MAX_NAME_CHARS: usize = 80;

/// Service ids of the energy transfer services (ISO 15118-20:2022 Table 204; unchecked).
pub mod service_id {
    pub const AC: u16 = 1;
    pub const DC: u16 = 2;
    pub const WPT: u16 = 3;
    pub const DC_ACDP: u16 = 4;
    pub const AC_BPT: u16 = 5;
    pub const DC_BPT: u16 = 6;
    pub const DC_ACDP_BPT: u16 = 7;
    pub const INTERNET: u16 = 65;
    pub const PARKING_STATUS: u16 = 66;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    pub service_id: u16,
    pub free: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedService {
    pub service_id: u16,
    pub parameter_set_id: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceDiscoveryReq<'a> {
    pub header: MessageHeader,
    /// Optional filter, 1..=16 ids.
    pub supported_service_ids: Option<&'a [u16]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceDiscoveryRes<'a> {
    pub header: MessageHeader,
    pub response_code: ResponseCode,
    pub service_renegotiation_supported: bool,
    /// 1..=8 entries.
    pub energy_transfer_services: &'a [Service],
    /// Value-added services, 1..=8 entries when present.
    pub vas: Option<&'a [Service]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceDetailReq {
    pub header: MessageHeader,
    pub service_id: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterValue<'a> {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Rational { exponent: i8, value: i16 },
    Text(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parameter<'a> {
    pub name: &'a str,
    pub value: ParameterValue<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterSet<'a> {
    pub id: u16,
    /// 1..=32 entries.
    pub parameters: &'a [Parameter<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceDetailRes<'a> {
    pub header: MessageHeader,
    pub response_code: ResponseCode,
    pub service_id: u16,
    /// 1..=32 entries.
    pub parameter_sets: &'a [ParameterSet<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceSelectionReq<'a> {
    pub header: MessageHeader,
    pub energy_transfer_service: SelectedService,
    /// 1..=16 entries when present.
    pub vas: Option<&'a [SelectedService]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceSelectionRes {
    pub header: MessageHeader,
    pub response_code: ResponseCode,
}

// ---------------------------------------------------------------- string table

const TABLE_CAP: usize = 64;

#[derive(Clone, Copy)]
enum Part {
    Name = 0,
    Text = 1,
}

/// EXI string table for one stream. Every literal goes into the global partition and the
/// local partition of its qname (here: the `Name` attribute, or `finiteString`). Hits, local
/// or global, change nothing (as EXIficient does; see the `service_detail_res_hits` vector). In a counting pass the strings are `""`
/// placeholders (only sizes matter for decoding).
struct Table<'s> {
    global: [&'s str; TABLE_CAP],
    glen: usize,
    local: [[u8; TABLE_CAP]; 2],
    llen: [usize; 2],
}

fn index_bits(size: usize) -> u32 {
    if size <= 1 { 0 } else { usize::BITS - (size - 1).leading_zeros() }
}

impl<'s> Table<'s> {
    fn new() -> Self {
        Self { global: [""; TABLE_CAP], glen: 0, local: [[0; TABLE_CAP]; 2], llen: [0; 2] }
    }

    fn add_local(&mut self, part: Part, global_index: usize) {
        let p = part as usize;
        self.local[p][self.llen[p]] = global_index as u8;
        self.llen[p] += 1;
    }

    fn push(&mut self, part: Part, s: &'s str) -> Result<(), DecodeError> {
        if self.glen == TABLE_CAP {
            return Err(DecodeError::Unsupported("string table full"));
        }
        self.global[self.glen] = s;
        self.add_local(part, self.glen);
        self.glen += 1;
        Ok(())
    }

    fn encode(&mut self, w: &mut BitWriter<'_>, part: Part, s: &'s str) -> Result<(), EncodeError> {
        let chars = s.chars().count();
        if chars == 0 {
            // See `encode_identifier`: empty string elements are not understood yet.
            return Err(EncodeError::InvalidValue("empty string not supported yet"));
        }
        if chars > MAX_NAME_CHARS {
            return Err(EncodeError::InvalidValue("string longer than 80 characters"));
        }
        let p = part as usize;
        if let Some(i) = self.local[p][..self.llen[p]].iter().position(|&g| self.global[usize::from(g)] == s) {
            w.uint(0)?;
            return w.bits(i as u32, index_bits(self.llen[p]));
        }
        if let Some(g) = self.global[..self.glen].iter().position(|e| *e == s) {
            w.uint(1)?;
            w.bits(g as u32, index_bits(self.glen))?;
            return Ok(());
        }
        if self.glen == TABLE_CAP {
            return Err(EncodeError::InvalidValue("too many distinct strings"));
        }
        w.uint(chars as u32 + 2)?;
        for c in s.chars() {
            w.uint(c as u32)?;
        }
        self.global[self.glen] = s;
        self.add_local(part, self.glen);
        self.glen += 1;
        Ok(())
    }

    fn decode(
        &mut self,
        r: &mut BitReader<'_>,
        part: Part,
        arena: Option<&'s Arena<'_>>,
    ) -> Result<&'s str, DecodeError> {
        let p = part as usize;
        match r.uint()? {
            0 => {
                let i = r.bits(index_bits(self.llen[p]))? as usize;
                let g = *self.local[p][..self.llen[p]].get(i).ok_or(DecodeError::InvalidValue("string table index"))?;
                Ok(self.global[usize::from(g)])
            }
            1 => {
                let g = r.bits(index_bits(self.glen))? as usize;
                if g >= self.glen {
                    return Err(DecodeError::InvalidValue("string table index"));
                }
                Ok(self.global[g])
            }
            n => {
                let len = (n - 2) as usize;
                if len == 0 {
                    return Err(DecodeError::Unsupported("empty string element"));
                }
                if len > MAX_NAME_CHARS {
                    return Err(DecodeError::InvalidValue("string longer than 80 characters"));
                }
                let s = exi::text::decode_string(r, len, arena)?;
                self.push(part, s)?;
                Ok(s)
            }
        }
    }
}

// ---------------------------------------------------------------- encode helpers

/// `SE(x)` with `se_bits`-wide code 0, CH, unsigned short, EE.
fn put_u16(w: &mut BitWriter<'_>, se_bits: u32, v: u16) -> Result<(), EncodeError> {
    w.bits(0, se_bits)?;
    w.bits(0, 1)?; // CH
    w.uint(u32::from(v))?;
    w.bits(0, 1) // EE
}

fn put_bool(w: &mut BitWriter<'_>, se_bits: u32, v: bool) -> Result<(), EncodeError> {
    w.bits(0, se_bits)?;
    w.bits(0, 1)?;
    w.bits(u32::from(v), 1)?;
    w.bits(0, 1)
}

/// An unrolled `maxOccurs` list including its end-of-list code; see the module docs.
fn put_repeats<T>(
    w: &mut BitWriter<'_>,
    items: &[T],
    max: usize,
    mut body: impl FnMut(&mut BitWriter<'_>, &T) -> Result<(), EncodeError>,
) -> Result<(), EncodeError> {
    if items.is_empty() || items.len() > max {
        return Err(EncodeError::InvalidValue("list length outside the schema's range"));
    }
    w.bits(0, 1)?; // SE of the first entry
    for (i, item) in items.iter().enumerate() {
        body(w, item)?;
        if i + 1 < max {
            w.bits(u32::from(i + 1 == items.len()), 2)?;
        } else {
            w.bits(0, 1)?;
        }
    }
    Ok(())
}

fn put_service(w: &mut BitWriter<'_>, s: &Service) -> Result<(), EncodeError> {
    put_u16(w, 1, s.service_id)?;
    put_bool(w, 1, s.free)?;
    w.bits(0, 1) // EE(Service)
}

fn put_selected(w: &mut BitWriter<'_>, s: &SelectedService) -> Result<(), EncodeError> {
    put_u16(w, 1, s.service_id)?;
    put_u16(w, 1, s.parameter_set_id)?;
    w.bits(0, 1) // EE(SelectedService)
}

fn begin<'o>(root: u32, header: &MessageHeader, out: &'o mut [u8]) -> Result<BitWriter<'o>, EncodeError> {
    let mut w = start(out)?;
    w.bits(root, ROOT_BITS)?;
    encode_header(&mut w, header)?;
    Ok(w)
}

fn finish(mut w: BitWriter<'_>) -> Result<usize, EncodeError> {
    w.bits(0, 1)?; // EE(message)
    Ok(1 + w.finish()?)
}

// ---------------------------------------------------------------- encode

pub fn encode_service_discovery_req(msg: &ServiceDiscoveryReq<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = begin(ROOT_SERVICE_DISCOVERY_REQ, &msg.header, out)?;
    match msg.supported_service_ids {
        Some(ids) => {
            w.bits(0, 2)?; // SE(SupportedServiceIDs)
            put_repeats(&mut w, ids, MAX_SERVICE_IDS, |w, id| {
                w.bits(0, 1)?; // CH
                w.uint(u32::from(*id))?;
                w.bits(0, 1) // EE
            })?;
        }
        None => w.bits(1, 2)?, // EE(ServiceDiscoveryReq)
    }
    if msg.supported_service_ids.is_some() { finish(w) } else { Ok(1 + w.finish()?) } // no EE(req) after the code-1 EE
}

pub fn encode_service_discovery_res(msg: &ServiceDiscoveryRes<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = begin(ROOT_SERVICE_DISCOVERY_RES, &msg.header, out)?;
    encode_response_code(&mut w, msg.response_code)?;
    put_bool(&mut w, 1, msg.service_renegotiation_supported)?;
    w.bits(0, 1)?; // SE(EnergyTransferServiceList)
    put_repeats(&mut w, msg.energy_transfer_services, MAX_SERVICES, put_service)?;
    match msg.vas {
        Some(vas) => {
            w.bits(0, 2)?; // SE(VASList)
            put_repeats(&mut w, vas, MAX_SERVICES, put_service)?;
            finish(w)
        }
        None => {
            w.bits(1, 2)?; // EE(ServiceDiscoveryRes)
            Ok(1 + w.finish()?)
        }
    }
}

pub fn encode_service_detail_req(msg: &ServiceDetailReq, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = begin(ROOT_SERVICE_DETAIL_REQ, &msg.header, out)?;
    put_u16(&mut w, 1, msg.service_id)?;
    finish(w)
}

pub fn encode_service_detail_res(msg: &ServiceDetailRes<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = begin(ROOT_SERVICE_DETAIL_RES, &msg.header, out)?;
    encode_response_code(&mut w, msg.response_code)?;
    put_u16(&mut w, 1, msg.service_id)?;
    w.bits(0, 1)?; // SE(ServiceParameterList)
    let mut table = Table::new();
    put_repeats(&mut w, msg.parameter_sets, MAX_PARAMETER_SETS, |w, set| {
        put_u16(w, 1, set.id)?;
        put_repeats(w, set.parameters, MAX_PARAMETERS, |w, p| put_parameter(w, &mut table, p))
    })?;
    finish(w)
}

fn put_parameter<'s>(w: &mut BitWriter<'_>, table: &mut Table<'s>, p: &Parameter<'s>) -> Result<(), EncodeError> {
    w.bits(0, 1)?; // AT(Name)
    table.encode(w, Part::Name, p.name)?;
    match p.value {
        ParameterValue::Bool(v) => {
            w.bits(0, 3)?;
            w.bits(0, 1)?; // CH
            w.bits(u32::from(v), 1)?;
            w.bits(0, 1)?;
        }
        ParameterValue::Byte(v) => {
            w.bits(1, 3)?;
            w.bits(0, 1)?;
            w.bits((i32::from(v) + 128) as u32, 8)?;
            w.bits(0, 1)?;
        }
        ParameterValue::Short(v) => {
            w.bits(2, 3)?;
            w.bits(0, 1)?;
            w.int(i32::from(v))?;
            w.bits(0, 1)?;
        }
        ParameterValue::Int(v) => {
            w.bits(3, 3)?;
            w.bits(0, 1)?;
            w.int(v)?;
            w.bits(0, 1)?;
        }
        ParameterValue::Rational { exponent, value } => {
            w.bits(4, 3)?;
            w.bits(0, 2)?; // SE(Exponent), CH
            w.bits((i32::from(exponent) + 128) as u32, 8)?;
            w.bits(0, 3)?; // EE, SE(Value), CH
            w.int(i32::from(value))?;
            w.bits(0, 2)?; // EE(Value), EE(rationalNumber)
        }
        ParameterValue::Text(s) => {
            w.bits(5, 3)?;
            w.bits(0, 1)?;
            table.encode(w, Part::Text, s)?;
            w.bits(0, 1)?;
        }
    }
    w.bits(0, 1) // EE(Parameter)
}

pub fn encode_service_selection_req(msg: &ServiceSelectionReq<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = begin(ROOT_SERVICE_SELECTION_REQ, &msg.header, out)?;
    w.bits(0, 1)?; // SE(SelectedEnergyTransferService)
    put_selected(&mut w, &msg.energy_transfer_service)?;
    match msg.vas {
        Some(vas) => {
            w.bits(0, 2)?; // SE(SelectedVASList)
            put_repeats(&mut w, vas, MAX_SELECTED_SERVICES, put_selected)?;
            finish(w)
        }
        None => {
            w.bits(1, 2)?;
            Ok(1 + w.finish()?)
        }
    }
}

pub fn encode_service_selection_res(msg: &ServiceSelectionRes, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = begin(ROOT_SERVICE_SELECTION_RES, &msg.header, out)?;
    encode_response_code(&mut w, msg.response_code)?;
    finish(w)
}

// ---------------------------------------------------------------- decode helpers

/// `SE(x)` with `se_bits`-wide code 0, CH, unsigned short, EE.
fn get_u16(r: &mut BitReader<'_>, se_bits: u32) -> Result<u16, DecodeError> {
    expect_zero(r, se_bits + 1)?; // SE, CH
    let v = u16::try_from(r.uint()?).map_err(|_| DecodeError::InvalidValue("value exceeds unsignedShort"))?;
    expect_zero(r, 1)?;
    Ok(v)
}

fn get_bool(r: &mut BitReader<'_>, se_bits: u32) -> Result<bool, DecodeError> {
    expect_zero(r, se_bits + 1)?;
    let v = r.bit()?;
    expect_zero(r, 1)?;
    Ok(v)
}

/// After entry number `done` (1-based) of an unrolled list: is there another one?
fn more_entries(r: &mut BitReader<'_>, done: usize, max: usize) -> Result<bool, DecodeError> {
    if done < max {
        match r.bits(2)? {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(DecodeError::Unsupported("unexpected event code")),
        }
    } else {
        expect_zero(r, 1)?;
        Ok(false)
    }
}

/// Counts the entries of a list without keeping them (validating each with `skip`), then
/// rewinds so the caller can decode them into exactly-sized arena storage.
fn count_entries(
    r: &BitReader<'_>,
    max: usize,
    mut skip: impl FnMut(&mut BitReader<'_>) -> Result<(), DecodeError>,
) -> Result<usize, DecodeError> {
    let mut r = r.clone();
    expect_zero(&mut r, 1)?; // SE of the first entry
    let mut n = 0;
    loop {
        skip(&mut r)?;
        n += 1;
        if !more_entries(&mut r, n, max)? {
            return Ok(n);
        }
    }
}

/// Decodes a whole list into the arena: count first, then allocate exactly.
fn get_repeats<'s, T: Copy>(
    r: &mut BitReader<'_>,
    arena: &'s Arena<'_>,
    max: usize,
    mut skip: impl FnMut(&mut BitReader<'_>) -> Result<(), DecodeError>,
    mut entry: impl FnMut(&mut BitReader<'_>) -> Result<T, DecodeError>,
) -> Result<&'s [T], DecodeError> {
    let n = count_entries(r, max, &mut skip)?;
    expect_zero(r, 1)?; // SE of the first entry
    let items = arena.alloc_slice_with(n, |i| {
        let v = entry(r)?;
        if more_entries(r, i + 1, max)? != (i + 1 < n) {
            return Err(DecodeError::InvalidValue("list changed between passes"));
        }
        Ok(v)
    })?;
    Ok(items)
}

fn get_service(r: &mut BitReader<'_>) -> Result<Service, DecodeError> {
    let service_id = get_u16(r, 1)?;
    let free = get_bool(r, 1)?;
    expect_zero(r, 1)?; // EE(Service)
    Ok(Service { service_id, free })
}

fn get_selected(r: &mut BitReader<'_>) -> Result<SelectedService, DecodeError> {
    let service_id = get_u16(r, 1)?;
    let parameter_set_id = get_u16(r, 1)?;
    expect_zero(r, 1)?; // EE(SelectedService)
    Ok(SelectedService { service_id, parameter_set_id })
}

fn get_id(r: &mut BitReader<'_>) -> Result<u16, DecodeError> {
    expect_zero(r, 1)?; // CH
    let v = u16::try_from(r.uint()?).map_err(|_| DecodeError::InvalidValue("value exceeds unsignedShort"))?;
    expect_zero(r, 1)?; // EE
    Ok(v)
}

fn services<'s>(r: &mut BitReader<'_>, arena: &'s Arena<'_>) -> Result<&'s [Service], DecodeError> {
    get_repeats(r, arena, MAX_SERVICES, |r| get_service(r).map(drop), get_service)
}

// ---------------------------------------------------------------- decode

pub(crate) fn decode_service_discovery_req<'s>(
    r: &mut BitReader<'_>,
    arena: &'s Arena<'_>,
) -> Result<ServiceDiscoveryReq<'s>, DecodeError> {
    let header = decode_header(r)?;
    let supported_service_ids = match r.bits(2)? {
        0 => {
            let ids = get_repeats(r, arena, MAX_SERVICE_IDS, |r| get_id(r).map(drop), get_id)?;
            expect_zero(r, 1)?; // EE(ServiceDiscoveryReq)
            Some(ids)
        }
        1 => None,
        _ => return Err(DecodeError::Unsupported("unexpected event code")),
    };
    Ok(ServiceDiscoveryReq { header, supported_service_ids })
}

pub(crate) fn decode_service_discovery_res<'s>(
    r: &mut BitReader<'_>,
    arena: &'s Arena<'_>,
) -> Result<ServiceDiscoveryRes<'s>, DecodeError> {
    let header = decode_header(r)?;
    let response_code = decode_response_code(r)?;
    let service_renegotiation_supported = get_bool(r, 1)?;
    expect_zero(r, 1)?; // SE(EnergyTransferServiceList)
    let energy_transfer_services = services(r, arena)?;
    let vas = match r.bits(2)? {
        0 => {
            let list = services(r, arena)?;
            expect_zero(r, 1)?; // EE(ServiceDiscoveryRes)
            Some(list)
        }
        1 => None,
        _ => return Err(DecodeError::Unsupported("unexpected event code")),
    };
    Ok(ServiceDiscoveryRes { header, response_code, service_renegotiation_supported, energy_transfer_services, vas })
}

pub(crate) fn decode_service_detail_req(r: &mut BitReader<'_>) -> Result<ServiceDetailReq, DecodeError> {
    let header = decode_header(r)?;
    let service_id = get_u16(r, 1)?;
    expect_zero(r, 1)?; // EE(ServiceDetailReq)
    Ok(ServiceDetailReq { header, service_id })
}

pub(crate) fn decode_service_detail_res<'s>(
    r: &mut BitReader<'_>,
    arena: &'s Arena<'_>,
) -> Result<ServiceDetailRes<'s>, DecodeError> {
    let header = decode_header(r)?;
    let response_code = decode_response_code(r)?;
    let service_id = get_u16(r, 1)?;
    expect_zero(r, 1)?; // SE(ServiceParameterList)
    // The string table runs through the whole list, so counting a nested list needs the
    // table state at that point: one counting pass over everything, then the real one.
    let mut counting = r.clone();
    let mut table = Table::new();
    let mut set_sizes = [0u8; MAX_PARAMETER_SETS];
    expect_zero(&mut counting, 1)?; // SE(first ParameterSet)
    let mut n_sets = 0;
    loop {
        get_u16(&mut counting, 1)?; // ParameterSetID
        expect_zero(&mut counting, 1)?; // SE(first Parameter)
        let mut n = 0;
        loop {
            get_parameter(&mut counting, &mut table, None)?;
            n += 1;
            if !more_entries(&mut counting, n, MAX_PARAMETERS)? {
                break;
            }
        }
        set_sizes[n_sets] = n as u8;
        n_sets += 1;
        if !more_entries(&mut counting, n_sets, MAX_PARAMETER_SETS)? {
            break;
        }
    }
    expect_zero(&mut counting, 1)?; // EE(ServiceDetailRes)

    let mut table = Table::new();
    expect_zero(r, 1)?; // SE(first ParameterSet)
    let parameter_sets = arena.alloc_slice_with(n_sets, |i| {
        let id = get_u16(r, 1)?;
        expect_zero(r, 1)?; // SE(first Parameter)
        let size = usize::from(set_sizes[i]);
        let parameters = arena.alloc_slice_with(size, |j| {
            let p = get_parameter(r, &mut table, Some(arena))?;
            more_entries(r, j + 1, MAX_PARAMETERS)?;
            Ok::<_, DecodeError>(p)
        })?;
        more_entries(r, i + 1, MAX_PARAMETER_SETS)?;
        Ok::<_, DecodeError>(ParameterSet { id, parameters: &*parameters })
    })?;
    expect_zero(r, 1)?; // EE(ServiceDetailRes)
    Ok(ServiceDetailRes { header, response_code, service_id, parameter_sets })
}

fn get_parameter<'s>(
    r: &mut BitReader<'_>,
    table: &mut Table<'s>,
    arena: Option<&'s Arena<'_>>,
) -> Result<Parameter<'s>, DecodeError> {
    expect_zero(r, 1)?; // AT(Name)
    let name = table.decode(r, Part::Name, arena)?;
    let value = match r.bits(3)? {
        0 => {
            expect_zero(r, 1)?;
            let v = r.bit()?;
            expect_zero(r, 1)?;
            ParameterValue::Bool(v)
        }
        1 => {
            expect_zero(r, 1)?;
            let v = r.bits(8)? as i32 - 128;
            expect_zero(r, 1)?;
            ParameterValue::Byte(v as i8)
        }
        2 => {
            expect_zero(r, 1)?;
            let v = i16::try_from(r.int()?).map_err(|_| DecodeError::InvalidValue("value exceeds short"))?;
            expect_zero(r, 1)?;
            ParameterValue::Short(v)
        }
        3 => {
            expect_zero(r, 1)?;
            let v = r.int()?;
            expect_zero(r, 1)?;
            ParameterValue::Int(v)
        }
        4 => {
            expect_zero(r, 2)?; // SE(Exponent), CH
            let exponent = (r.bits(8)? as i32 - 128) as i8;
            expect_zero(r, 3)?; // EE, SE(Value), CH
            let value = i16::try_from(r.int()?).map_err(|_| DecodeError::InvalidValue("value exceeds short"))?;
            expect_zero(r, 2)?; // EE(Value), EE(rationalNumber)
            ParameterValue::Rational { exponent, value }
        }
        5 => {
            expect_zero(r, 1)?;
            let s = table.decode(r, Part::Text, arena)?;
            expect_zero(r, 1)?;
            ParameterValue::Text(s)
        }
        _ => return Err(DecodeError::Unsupported("unexpected event code")),
    };
    expect_zero(r, 1)?; // EE(Parameter)
    Ok(Parameter { name, value })
}

pub(crate) fn decode_service_selection_req<'s>(
    r: &mut BitReader<'_>,
    arena: &'s Arena<'_>,
) -> Result<ServiceSelectionReq<'s>, DecodeError> {
    let header = decode_header(r)?;
    expect_zero(r, 1)?; // SE(SelectedEnergyTransferService)
    let energy_transfer_service = get_selected(r)?;
    let vas = match r.bits(2)? {
        0 => {
            let list = get_repeats(r, arena, MAX_SELECTED_SERVICES, |r| get_selected(r).map(drop), get_selected)?;
            expect_zero(r, 1)?; // EE(ServiceSelectionReq)
            Some(list)
        }
        1 => None,
        _ => return Err(DecodeError::Unsupported("unexpected event code")),
    };
    Ok(ServiceSelectionReq { header, energy_transfer_service, vas })
}

pub(crate) fn decode_service_selection_res(r: &mut BitReader<'_>) -> Result<ServiceSelectionRes, DecodeError> {
    let header = decode_header(r)?;
    let response_code = decode_response_code(r)?;
    expect_zero(r, 1)?; // EE(ServiceSelectionRes)
    Ok(ServiceSelectionRes { header, response_code })
}
