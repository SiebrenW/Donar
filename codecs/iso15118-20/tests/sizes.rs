//! Messages live on the stack while they are built or inspected, and the largest ones nest
//! deeply. Lists are arena slices, so even they stay small; this keeps that true.

use iso15118_20::{common, dc};
use std::mem::size_of;

#[test]
fn messages_stay_small() {
    let sizes = [
        ("common::Message", size_of::<common::Message<'_>>()),
        ("dc::Message", size_of::<dc::Message<'_>>()),
        ("ScheduleExchangeReq", size_of::<common::ScheduleExchangeReq<'_>>()),
        ("ScheduleExchangeRes", size_of::<common::ScheduleExchangeRes<'_>>()),
        ("PowerDeliveryReq", size_of::<common::PowerDeliveryReq<'_>>()),
        ("DcChargeLoopReq", size_of::<dc::DcChargeLoopReq>()),
        ("DcChargeLoopRes", size_of::<dc::DcChargeLoopRes<'_>>()),
    ];
    for (name, size) in sizes {
        println!("{name}: {size} bytes");
        assert!(size <= 2048, "{name} is {size} bytes");
    }
}
