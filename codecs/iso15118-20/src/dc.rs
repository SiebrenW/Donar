//! `V2G_CI_DC` messages (payload type `0x8004`). The DC grammar is separate from the common
//! one: its document root holds 49 elements (6-bit code), CPDReq = 15 and CPDRes = 16
//! (checked against EXIficient). BPT variants are not implemented.

use exi::bits::BitReader;
use crate::common::*;
use exi::{DecodeError, EncodeError};

const ROOT_CPD_REQ: u32 = 15;
const ROOT_CPD_RES: u32 = 16;

/// `DC_ChargeParameterDiscoveryReq` with the plain (non-BPT) energy transfer mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeParameterDiscoveryReq {
    pub header: MessageHeader,
    pub maximum_charge_power: RationalNumber,
    pub minimum_charge_power: RationalNumber,
    pub maximum_charge_current: RationalNumber,
    pub minimum_charge_current: RationalNumber,
    pub maximum_voltage: RationalNumber,
    pub minimum_voltage: RationalNumber,
    /// 0..=100
    pub target_soc: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeParameterDiscoveryRes {
    pub header: MessageHeader,
    pub response_code: ResponseCode,
    pub maximum_charge_power: RationalNumber,
    pub minimum_charge_power: RationalNumber,
    pub maximum_charge_current: RationalNumber,
    pub minimum_charge_current: RationalNumber,
    pub maximum_voltage: RationalNumber,
    pub minimum_voltage: RationalNumber,
    pub power_ramp_limitation: Option<RationalNumber>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    ChargeParameterDiscoveryReq(ChargeParameterDiscoveryReq),
    ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes),
}

/// The energy transfer mode is a substitution group: BPT = 0, plain DC = 1 (2-bit code).
const MODE_DC: u32 = 1;

pub fn encode_charge_parameter_discovery_req(
    msg: &ChargeParameterDiscoveryReq,
    out: &mut [u8],
) -> Result<usize, EncodeError> {
    if msg.target_soc.is_some_and(|s| s > 100) {
        return Err(EncodeError::InvalidValue("TargetSOC above 100"));
    }
    let mut w = start(out)?;
    w.bits(ROOT_CPD_REQ, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    w.bits(MODE_DC, 2)?; // SE(DC_CPDReqEnergyTransferMode)
    for f in [
        msg.maximum_charge_power,
        msg.minimum_charge_power,
        msg.maximum_charge_current,
        msg.minimum_charge_current,
        msg.maximum_voltage,
        msg.minimum_voltage,
    ] {
        put_rational(&mut w, 0, 1, f)?;
    }
    match msg.target_soc {
        Some(soc) => {
            w.bits(0, 3)?; // SE(TargetSOC) in a 2-bit state, CH
            w.bits(u32::from(soc), 7)?; // percentValueType: 0..=100 as a 7-bit offset
            w.bits(0, 1)?; // EE(TargetSOC)
            w.bits(0, 1)?; // EE(mode)
        }
        None => w.bits(1, 2)?, // EE(mode)
    }
    w.bits(0, 1)?; // EE(DC_ChargeParameterDiscoveryReq)
    Ok(1 + w.finish()?)
}

pub fn encode_charge_parameter_discovery_res(
    msg: &ChargeParameterDiscoveryRes,
    out: &mut [u8],
) -> Result<usize, EncodeError> {
    let mut w = start(out)?;
    w.bits(ROOT_CPD_RES, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    encode_response_code(&mut w, msg.response_code)?;
    w.bits(MODE_DC, 2)?; // SE(DC_CPDResEnergyTransferMode)
    for f in [
        msg.maximum_charge_power,
        msg.minimum_charge_power,
        msg.maximum_charge_current,
        msg.minimum_charge_current,
        msg.maximum_voltage,
        msg.minimum_voltage,
    ] {
        put_rational(&mut w, 0, 1, f)?;
    }
    match msg.power_ramp_limitation {
        Some(ramp) => {
            put_rational(&mut w, 0, 2, ramp)?; // SE(EVSEPowerRampLimitation) in a 2-bit state
            w.bits(0, 1)?; // EE(mode)
        }
        None => w.bits(1, 2)?, // EE(mode)
    }
    w.bits(0, 1)?; // EE(DC_ChargeParameterDiscoveryRes)
    Ok(1 + w.finish()?)
}

pub fn decode(buf: &[u8]) -> Result<Message, DecodeError> {
    let (&header_byte, body) = buf.split_first().ok_or(DecodeError::UnexpectedEof)?;
    if header_byte != HEADER {
        return Err(DecodeError::Unsupported("EXI header with options or cookie"));
    }
    let mut r = BitReader::new(body);
    match r.bits(ROOT_BITS)? {
        ROOT_CPD_REQ => {
            let header = decode_header(&mut r)?;
            expect_mode(&mut r)?;
            let v = get_six(&mut r)?;
            let target_soc = match r.bits(2)? {
                0 => {
                    expect_zero(&mut r, 1)?; // CH
                    let soc = r.bits(7)?;
                    if soc > 100 {
                        return Err(DecodeError::InvalidValue("TargetSOC above 100"));
                    }
                    expect_zero(&mut r, 2)?; // EE(TargetSOC), EE(mode)
                    Some(soc as u8)
                }
                1 => None,
                _ => return Err(DecodeError::Unsupported("unexpected event code")),
            };
            expect_zero(&mut r, 1)?; // EE(DC_ChargeParameterDiscoveryReq)
            Ok(Message::ChargeParameterDiscoveryReq(ChargeParameterDiscoveryReq {
                header,
                maximum_charge_power: v[0],
                minimum_charge_power: v[1],
                maximum_charge_current: v[2],
                minimum_charge_current: v[3],
                maximum_voltage: v[4],
                minimum_voltage: v[5],
                target_soc,
            }))
        }
        ROOT_CPD_RES => {
            let header = decode_header(&mut r)?;
            let response_code = decode_response_code(&mut r)?;
            expect_mode(&mut r)?;
            let v = get_six(&mut r)?;
            let power_ramp_limitation = match r.bits(2)? {
                0 => {
                    let ramp = get_rational_after_se(&mut r)?;
                    expect_zero(&mut r, 1)?; // EE(mode)
                    Some(ramp)
                }
                1 => None,
                _ => return Err(DecodeError::Unsupported("unexpected event code")),
            };
            expect_zero(&mut r, 1)?; // EE(DC_ChargeParameterDiscoveryRes)
            Ok(Message::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                header,
                response_code,
                maximum_charge_power: v[0],
                minimum_charge_power: v[1],
                maximum_charge_current: v[2],
                minimum_charge_current: v[3],
                maximum_voltage: v[4],
                minimum_voltage: v[5],
                power_ramp_limitation,
            }))
        }
        _ => Err(DecodeError::Unsupported("message not implemented")),
    }
}

fn expect_mode(r: &mut BitReader<'_>) -> Result<(), DecodeError> {
    match r.bits(2)? {
        MODE_DC => Ok(()),
        0 => Err(DecodeError::Unsupported("BPT energy transfer mode")),
        _ => Err(DecodeError::Unsupported("unexpected event code")),
    }
}

fn get_six(r: &mut BitReader<'_>) -> Result<[RationalNumber; 6], DecodeError> {
    let mut v = [RationalNumber { exponent: 0, value: 0 }; 6];
    for slot in &mut v {
        *slot = get_rational(r, 1)?;
    }
    Ok(v)
}

/// The rational after its `SE` code was already consumed.
fn get_rational_after_se(r: &mut BitReader<'_>) -> Result<RationalNumber, DecodeError> {
    expect_zero(r, 2)?;
    let exponent = (r.bits(8)? as i32 - 128) as i8;
    expect_zero(r, 3)?;
    let value = i16::try_from(r.int()?).map_err(|_| DecodeError::InvalidValue("value exceeds short"))?;
    expect_zero(r, 2)?;
    Ok(RationalNumber { exponent, value })
}
