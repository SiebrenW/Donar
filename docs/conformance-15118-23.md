# ISO/PAS 15118-23 conformance map

ISO/PAS 15118-23:2026 is the abstract test suite for ISO 15118-20 (DC and ACDP; 70 test cases, `TC23_SECC_*` for the charger, `TC23_EVCC_*` for the vehicle). This file maps its test case families to our tests. It is a map for planning, not a certification claim.

How it was made: by the test purposes and referenced requirements in the PAS, against the names and comments of our tests. The assertions were **not** traced one by one. "Covered" means a test of ours exercises the behaviour; it does not mean the PAS's exact stimulus (timing, default parameter set) is reproduced.

Caveats:

- The PAS takes its test system, default parameter sets (ISO 15118-21:2025, Annex C) and timer handling (ISO 15118-21:2025, 6.5) from ISO 15118-21, which we do not have. Running the cases as written needs it.
- ACDP (ISO 15118-20 ACDP): only the EXI codec exists, no session logic; the `ett_value := ACDP` branches do not apply.
- The PAS cases cover DC; our AC and ISO 15118-2/DIN work has no counterpart in it.

Status: **covered**, **partly** (behaviour exists, the stimulus or one verdict criterion is missing), **open** (no test), **n/a**.

## Charger (SUT = our `evse-core` behind `evse-sim`)

| Family (cases) | Requirements | Status | Our tests |
|---|---|---|---|
| SessionSetup_001, ServiceDetail_001/002, ScheduleExchange_001 | V2G20-1486, 1357, 1360, 1611, 457 | covered | `against_evcc` full DC/BPT sessions, `session_flow` |
| ChargeParameterDiscovery_001 (unknown session id) | V2G20-460, 736 | partly | session id checks in `evse-core` (`a_session_stop_with_another_session_id_is_refused...`); not for every message type |
| CableCheck_001/002 (`Ongoing` first, then `Finished`) | V2G20-2003, 2004, 916, 917 | covered | `Ongoing` comes from the host's own cable check (`Secc::cable_check`) once C/D is there; without C/D the answer is `FAILED` at once (CableCheck_005) |
| CableCheck_003, PreCharge_003, WeldingDetection_003 (out-of-sequence request gives `FAILED_SequenceError`) | V2G20-459, 538, 736 | covered | sequence checks in `evse-core` |
| CableCheck_004, PreCharge_004, ChargeLoop_002, WeldingDetection_004 (unknown session) | V2G20-460, 736 | partly | see ChargeParameterDiscovery_001 |
| CableCheck_005 (`FAILED` without C/D), WeldingDetectionOrSessionStop_001 (`FAILED` without B) | V2G20-916, 918..922, 736 | covered | `a_cable_check_without_c_or_d_is_answered_failed`, `a_welding_detection_without_b_is_answered_failed` |
| CableCheck_006, PreCharge_005, ChargeLoop_003, WeldingDetection_005 (sequence timeout ends the session) | V2G20-441, 443, 1481, 034, 1502 | covered | `a_vehicle_that_goes_quiet_for_the_sequence_timeout_ends_the_session_as_such` |
| CableCheck_007, PreCharge_006 (SessionStopReq for a non-critical reason is answered) | V2G20-2645, 1632 | covered | `a_session_stop_is_taken_wherever_the_power_is_not_on` |
| PreCharge_001/002, ChargeLoop_001, WeldingDetection_001/002 | V2G20-2005, 2006, 2013, 1630, 921, 1631 | covered | `against_evcc` full sessions |
| ChargeLoop_004/005 (absolute limit values) | V2G20-2654, 2655 | covered | `limits.rs` checks, `loopback` violation tests |
| SessionStop_001 (duty cycle 3 to 7 % for the whole session) | V2G20-1408 | covered | The charger's target is 5 % (`evse-sim` writes 5 and keeps it); a measurement from 3 to 7 % counts as 5 % (`Pwm::is_five_percent`, the vehicle's DC guard: `a_measurement_from_3_to_7_percent_is_five_percent`) |
| V2GTPDCMessages_001/002 (header, TCP port ranges) | V2G20-2307, 078, 080 | partly | header in `v2gtp` tests; ports are the host's and untested |
| V2GTPDCMessages_003..008 (invalid header is ignored) | V2G20-800 | partly | ignored since the V2G20-800 change: `a_frame_with_a_wrong_v2gtp_header_is_ignored` (version, inverse version, payload type, empty payload) and `garbage_is_rejected_without_panicking`; a payload length that is too small or too large cannot be told from a short read, so it is not resynchronised (the stream waits or the sequence timeout ends it); no cable-level case |

## Vehicle (SUT = our `evcc-core` behind `evcc-sim`)

| Family (cases) | Requirements | Status | Our tests |
|---|---|---|---|
| AuthorizationSetup_001, CableCheck_001/002, PreCharge_001..003, PowerDelivery_001, ChargeLoop_001..003, WeldingDetection_001..003, SessionStop_001 (the right request at the right time) | V2G20-1749, 1594, 1431, 1432..1435, 1438, 1441, 1448, 1573, 1461, 1462 | covered | `session_flow`: `full_flow_to_authorized`, `cable_check_pre_charge_and_power_delivery`, `the_charge_loop_cycles_and_stops_on_request` |
| PowerDelivery_002 (stop within `SECC_StopEnergyTransfer_Timeout` after the customer's action) | V2G20-1444 | partly | `a_stop_request_during_the_loop_interval_acts_at_once`; the timeout bound itself is not asserted |
| CableCheck_003 (CP C or D before the CableCheckReq), WeldingDetectionOrDisconnect_001 (CP B before the request) | V2G20-912, 913 | covered | `the_vehicle_asks_for_the_pilot_around_the_cable_check_and_the_welding_detection` |
| CableCheck_004, PreCharge_004, ChargeLoop_004, WeldingDetection_004 (message timeout) | V2G20-436, 438, 1966, 025, 1501 | covered | `every_service_step_has_a_response_timeout`, `the_charge_loop_has_a_half_second_timeout_and_no_ongoing_bound` |
| CableCheck_005, PreCharge_005 (cable check / precharge timeout) | V2G20-700, 704, 706, 1398, 1774 | covered | `cable_check_and_pre_charge_have_their_own_timeouts` |
| CableCheck_006/007, PreCharge_007/008, ChargeLoop_005..009, WeldingDetection_005..007 (`FAILED*` ends the session) | V2G20-2220, 728, 1967, 1968, 025 | covered | `dc_failures_end_the_session` |
| PreCharge_006 (waits for the cable check) | V2G20-1397 | covered | `cable_check_pre_charge_and_power_delivery`, `ongoing_answers_are_polled_at_the_configured_pace` |
| ChargeLoop_010/011 (absolute values), ChargeLoop_012 (BPT dynamic parameters) | V2G20-2198, 2199, 2679, 2680 | partly | 2198/2199 covered in `limits.rs`; 2679/2680 (EVMaximumV2X...) not checked |
| V2GTPDCMessages_001/002 (header, source port range) | V2G20-2307, 077, 079 | partly | see the charger side |
| V2GTPDCMessages_003..008 (invalid header ignored) | V2G20-800 | partly | `a_frame_with_a_wrong_v2gtp_header_is_ignored` in `session_flow` (same limits as the charger side) |

## What this shows

1. Most of the vehicle and charger sequence behaviour of the PAS is exercised by our flow tests. The gaps are host-level: the V2GTP header and port cases at cable level, the duty cycle range, and the duty cycle.
2. To run the cases as the PAS describes them we need ISO 15118-21 (default parameter sets, timers) and a tester role; the scenarios runner could play the test system (a scripted partner that sends the PAS's stimuli and checks verdicts), but that is a larger piece of work.
3. Requirement ids above were taken from the PAS's test case tables; they have not been re-checked against ISO 15118-20 text here, so code comments citing them still follow the rule in AGENT.md.

## Next steps (not started)

- Obtain ISO 15118-21 and see whether the scenario runner can take the tester role.
