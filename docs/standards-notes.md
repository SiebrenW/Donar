# Gaps and inconsistencies in the standards

What we ran into while implementing ISO 15118-20, ISO 15118-2, DIN SPEC 70121 and SLAC (ISO 15118-3), and what the code does about it. Requirement ids are those of the standard's text (`V2G20-…` for ISO 15118-20:2022, `V2G-DC-…` for DIN SPEC 70121). "Decision" is what the code does today; where it is a guess it says so, and the guess can be checked against another implementation or a standards body answer.

Terminology used here: **V2G** is the high level communication, **BPT**/**V2X** is power delivered from the vehicle (see `AGENT.md`).

Three kinds of entry: the **text contradicts itself** (or a standard it refers to), the standard is **silent** where an implementation has to choose, and it asks for something that **cannot be done** as written. We do not list things that are only a consequence of the format: an element that is optional in the schema and required in some cases by the text (such as the display parameters under `MobilityNeedsMode` 2) is a normal use of optional elements, not an inconsistency. A last section lists where *other implementations* deviate from the standard, because those were found by testing against them and are easy to mistake for ours.

## ISO 15118-20:2022

### The text contradicts itself

| Where | What | Decision |
|---|---|---|
| `TimeAnchor` (Table 112; 8.3.5.3.9, Annex J examples) | Table 112 says milliseconds; the examples and 8.3.5.3.9 read as microseconds. | Microseconds of SECC time. |
| V2G20-1032, 2098 against V2G20-1478, 1644, 1956 | After a wake-up from a pause, charge parameter discovery is the next request; elsewhere the vehicle selects the services again. | Charge parameter discovery is next; authorization and service selection carry over. |
| V2G20-2121 to 2123, 1068 | The text has the vehicle send a `PowerDeliveryReq` with `ChargeProgress` "Pause", but the schema's values are Start, Stop, Standby and ScheduleRenegotiation. V2G20-2123 names Stop for a vehicle that wants to start with a pause. | `GoToPause`: skip the cable check and pre-charge, send Stop, then `SessionStopReq(Pause)`. |
| V2G20-479 | Names the code `FAILED_TariffSelectionInvalid` for an unknown schedule tuple; the schema has `FAILED_ScheduleSelectionInvalid` (and the tariff code is not one of the response codes). | `FAILED_ScheduleSelectionInvalid`. |
| V2G20-2584, 2598 against the certificate tables | "Shall not contain any further values" in the subject of a SECC or vehicle certificate, but the tables allow C, OU and DC. | The tables are followed; a domain component must be CSO or EV. |
| Annex C.3.2 | The SECCID example does not come out with the check digit the algorithm gives (4, not 2); the EMAID and EVCCID examples do. | The algorithm. |

### Conflicts with a referenced standard, or cannot be satisfied

| Where | What | Decision |
|---|---|---|
| Certificate tables of 2022 (Ed448 SECC certificates) | `keyAgreement` is required in the key usage, which an Ed448 key cannot do. Amd 1 (Table B.6) makes it 0/1. | 0/1 for Ed448, as the amendment has it. |
| Vehicle certificate tables of 2022: `AuthorityKeyIdentifier`, `SubjectKeyIdentifier` | Marked critical; RFC 5280 says they are not. Amd 1 makes them non-critical. | Either is accepted. |
| `AuthorityInfoAccess`, `CRLDistributionPoints` in the 2022 profiles | Critical; OpenSSL reports an unhandled critical extension. Amd 1 makes them non-critical for that reason (B.2.2). | Either is accepted; the TLS contexts let exactly those two through when critical. |
| OCSP in TLS 1.3 (V2G20-2411 to 2416) | Responses for every certificate of the chain, in the handshake. OpenSSL hands a client one stapled response (the leaf's) and has no API for the others. | `require_all` fails for a chain with a sub-CA unless the host brings more responses. The simulators check the leaf only. |

Not covered at all: cross certificates (V2G20-1002, 1005, 1853), the profiles of the provisioning, e-mobility, OEM provisioning, private environment and OCSP signer certificates, and CRLs.

### Silent where an implementation has to choose

| Where | What is not said | Decision |
|---|---|---|
| Schedule renegotiation side stream (payload type 0x8101, 8.6.5.1, Annex F) | Which schema the side stream uses: charge parameter discovery is in the DC schema, schedule exchange and power delivery in the common messages. | Messages are coded as on the main stream; the receiver decodes what the exchange expects next. No independent check exists (the Python library defines the payload type and does not implement it). |
| Message signatures (V2G20-1449) | Whether the digest and signature cover the EXI fragment *with* its header octet; the ECDSA value format. | With the header octet (as EXIficient-based stacks do), r‖s (xmldsig-more). |
| SHAKE256 output length (Amd 1 V2G20-3012) | The 2022 text names the digest algorithm only. Amd 1 fixes 114 octets, but words it for "signatures as defined by V2G20-2476", and 2476 is the `DigestMethod`; the Ed448 signature itself is RFC 8032, which uses 114 octets inside. | 114 octets for the digest. |
| EMAID and EVSEID (V2G20-2076, 2087; IEC 63119-2) | IEC 63119-2 refers to the IDACS format document for Europe (not at hand) and defines a "global identifier" (format issuer `EMI3` or `*GBT`, format id, original id) whose content it leaves to the issuer. Amd 1 only sets the length (64). | EMAID: the ISO 15118-2 Annex H.1 form with its check digit, or a global identifier of at most 64 characters. EVSEID: the ISO 15118-2 Annex H.2 form or a global identifier. The IDACS rules are not checked. |
| GoToPause with a non-zero first schedule entry (Amd 1 V2G20-3299, 3301) | What the vehicle does in Scheduled mode when `GoToPause` is TRUE and the first entry of a schedule has power. 3301 defines the stop only with Power 0 in the first entry of every schedule. | The vehicle follows `GoToPause` whatever the schedule says. |
| BPT service parameters (Table 207) | Whether `BPTChannel` and `GeneratorMode` are mandatory in a DC_BPT parameter set. The Python SECC offers none. | The vehicle accepts a set without them (`Config::optional_parameters`). Unchecked. |
| `EVMaximumV2XEnergyRequest`, `EVMinimumV2XEnergyRequest` (V2G20-2681, 2682, 2124, 2125) | Sign conventions are given in prose only ("a negative value indicates that PresentSOC is above the V2X range"). | Both values or none, clamped into minimum energy ≤ minimum V2X ≤ maximum V2X ≤ maximum energy. |
| Discharge in Dynamic mode | The convention for the target energy and power when discharging (a negative `EVTargetEnergyRequest`, a negative target active power). | Negative means discharge. Open item in PLAN. |
| `ScheduleExchangeRes` `Ongoing` | No ScheduleExchange-specific rule for the repeat timing. | V2G_EVCC_Ongoing_Timeout applies, as for other `Ongoing` answers. |

## ISO 15118-2 and DIN SPEC 70121

No entries yet. These were read less closely than ISO 15118-20; that is not evidence that they are consistent.

## ISO 15118-3 and SLAC

| Where | What | Decision |
|---|---|---|
| `CM_VALIDATE` | The MMTYPEs (0x6044, 0x6045) are left to the HomePlug specification. | Those of the open HomePlug implementations. Unchecked against hardware. |
| Network id and key on the modem | The NID octet layout of `CM_SET_KEY` and the vendor `VS_NW_INFO` answer are not in the standard. | The layout was read from captures on a QCA7005; a lone modem was checked, a joined modem was not. |
| Attenuation thresholds (10 dB direct, 20 dB indirect) | They assume a modem at the plug. Wiring between the plug and the modem adds attenuation that is no part of the path to the charger, per carrier group. The standard has no correction. | Configurable per group in the vehicle and the simulated charger (`attenuation_correction_db`). The transmit level (how loud the modem sounds) is the modem's and not configured. |
| Repeated `CM_SLAC_MATCH.REQ` | V2G3-A09-97 asks the charger to answer a repeated request from the same vehicle again; V2G3-A09-118 says that once "Matched" no further SLAC message is sent or processed. | The repeat is answered, nothing else is processed. |
| `Timer` of `CM_VALIDATE.REQ` | Table A.6 and HomePlug Green PHY Table 11-148 give 0x00 = 100 ms, 0x01 = 200 ms (time = (Timer + 1) x 100 ms); in the first request 0x00 means "step 1" (V2G3-A09-75, Table A.5), and a repeated first request is answered again (V2G3-A09-77). | The time is (Timer + 1) x 100 ms. A toggle sequence takes 600 ms or more, so 0x00 never starts a window; the charger tells the first step from the second by its state. (Before this was decided both sides read Timer x 100 ms, which two implementations that agree with the table would have disagreed with.) |
| Destination of the second `CM_VALIDATE.REQ` | Table A.6 says broadcast, Table A.5 unicast for the first. A charger may refuse a broadcast second request (it is a request for it). | The vehicle sends both to the charger's address; the charger takes either. |
| Report without a profile | V2G3-A09-36 has the vehicle ignore a report with `NumSounds` 0. The standard does not say what a charger reports whose modem gave no profile. | The largest attenuation and no sounds (a fixed measurement only where the host has no modem to ask: the simulator). A charger that reported a small attenuation because its modem was silent would be matched with a vehicle on the next socket. |
| Re-matching after `D-LINK_TERMINATE.request` | The standard has the module "Unmatched" afterwards and the vehicle has been told to end the session. | No matching process is served until the pilot starts the next one (V2G3-M06-11). |
| V2G3-M07-02 | The charger applies 5 % only when the module "is ready for communication (receive first SLAC message)", which it cannot know before the vehicle has seen the 5 %. | The module is ready when it answers its host (`T_conn_max_comm`); the oscillator is the host's. |
| `TT_EVSE_SLAC_init` after a failed match | V2G3-A09-12 starts it with the trigger only; the vehicle repeats a failed run for `TT_matching_repetition` (10 s) from its own trigger. | The window runs from the trigger and is not restarted by a failed process; a vehicle that retries after it ran out is not answered. |
| Measurement without a modem profile | V2G3-A09-43 has the charger "receive and process" `CM_ATTEN_PROFILE.IND`; a host without a modem that reports it (the simulator) has no profile. | The profiles are averaged when they come (V2G3-A09-19); without any, `Config::attenuation` stands in. |

## Other implementations that deviate from the standard

Found by testing against them; kept here so that a failure is not blamed on the wrong side.

| Implementation | Deviation |
|---|---|
| Python `iso15118` 0.31.2 (SECC) | Requires `PowerToleranceAcceptance` always. Requires `ChargingComplete` in `DisplayParameters`. Sends `TimeAnchor` 0 (the vehicle counts from receipt). Offers no `BPTChannel`/`GeneratorMode`. Offers `MobilityNeedsMode` 2 for AC only. Does not implement the schedule renegotiation side stream. Answers fixed values when discharging. |
| `pyslac` 0.8.3 | Needs a PLC chip (or a stand-in) for `CM_SET_KEY` and the attenuation profile; Its `environs` dependency breaks with `marshmallow` 4. |
| OpenV2G, libcbv2g (EXI codecs) | Re-encode some documents to different bytes (OpenV2G: certificate installation and update requests, one byte more) or fail to decode (libcbv2g: several DIN and ISO 15118-20 documents). The full list is in `crates/codec/exi-gen/benchmarks/RESULTS.md`. |
| OpenSSL | See the OCSP entry above. |

## Where this list is weak

- Items marked "unchecked" were not verified against a second implementation or hardware.
- The requirements were read from a text extraction of the PDFs; the response code tables (224 and 226) were read by meaning and should be re-checked against the PDF.
