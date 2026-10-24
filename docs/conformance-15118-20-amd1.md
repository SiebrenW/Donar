# ISO 15118-20:2022/Amd.1:2026 conformance map

Amendment 1 adds the AC DER service (IEC and SAE), the MCS service (megawatt charging, ISO 15118-10 10BASE-T1S), and changes the security concept and the certificate profiles. This file lists what it changes in what the code already does, and what is not done. It is a map for planning, not a certification claim.

How it was made: the amendment's change instructions for clauses 7 and 8 and Annexes B, C and H were read against the code; the profile tables of Annex B were compared on key usage, domain components, key identifiers and the revocation extensions. The new Annexes K (MCS), L (AC DER IEC) and M (AC DER SAE) were **not** read beyond their headings. Requirements that mention only a renumbering (the figures, the notes) were skipped.

Status: **done** (checked or changed in code, with a test), **met** (the behaviour was there), **open** (not done).

## Existing behaviour changed by the amendment

| Requirement | Change | Status | Where |
|---|---|---|---|
| V2G20-3012 | The SHAKE256 digest of an Ed448 signature is 114 octets (was a guess of 64) | done | `evcc_linux::xmlsig`, `MAX_DIGEST_LEN`; the Ed448 xmlsig test checks the length. Python's `iso15118` does not offer Ed448, so no partner to compare with. |
| V2G20-3000, 3001 (replace 1004) | A certificate is valid only within the validity of its issuer (the end too, not only the start) | done | `certificates::check_against_issuer` |
| V2G20-3431, 3432 (replace 1852, delete 1851) | Subject and authority key identifiers by method 1 *or* 2 | done | `is_key_identifier`; `Flaw::KeyIdMethod1` is now accepted, `Flaw::KeyIdWrong` is the violation |
| Tables B.3 to B.18 | `cRLSign` is 0 in every CA profile (was 1 in the sub-CAs) | done: either value accepted, since 2022 chains exist | `profile()`; the test PKI has sub-CA1 by the amendment and sub-CA2 by the old text |
| Tables B.5 ff. | `AuthorityInfoAccess` and `CRLDistributionPoints` are non-critical (were critical) | done: either accepted | `check_extensions`; see `standards-notes.md` |
| Table B.6 | Ed448 SECC certificate: `keyAgreement` is 0/1 (an Ed448 key cannot agree on a key) | done | `check_certificate` |
| V2G20-3406 | The subject of a sub-CA carries the domain component CSO, CPS, MSP, OEM or EV by its usage | done for a wrong value; a missing one is not flagged (2022 chains have none) | `Profile::domain_component`, `Flaw::WrongDomainComponent`. This also fixes the contract certificate, whose leaf was expected to have DC "EV". |
| V2G20-3046 ff. | `SubjectInfoAccess` (cross certificate indication) may be in sub-CAs | done | `profile()` |
| Annex C, V2G20-3083 to 3087 | EMAID, EVSEID at most 64 characters; PCID 18 to 64; SECCID 39 to 64; EVCCID 20 to 64; exactly one check digit | done | `check_emaid`, `check_evseid`, `check_pcid`, `check_secc_id`, `check_evcc_id`. IEC 63119-2 (global identifier) and ISO 15118-2 Annex H are covered, the IDACS format is not (not at hand). The EVSEID checks are not applied to a peer's `SessionSetupRes`, only to the simulator's defaults (a test). |
| V2G20-3299 to 3302, 3304, 3305 (replace 2121, 2122) | Pause directly after the schedule exchange: `GoToPause` TRUE with Power 0 and a duration in the first entry of every schedule (Scheduled mode); also for AC. The vehicle sends Stop (AC) or skips cable check and pre-charge (DC) | done in the simulator (Power 0 when `no_power`); the vehicle already followed `GoToPause` for AC and DC | `evse-sim` `schedule_entries`. The core charger leaves the schedule to the host. |
| V2G20-3308, 3317, 3318 (replace 1850) | `EVSENotification` Pause with `NotificationMaxDelay` > 0, repeated with the remaining time until the vehicle pauses or enters standby | done in the simulator; the vehicle already did both | `evse-sim` `pause_asked` |
| V2G20-3295 to 3297 (replace 1448, 1462) | Welding detection `Ongoing` until finished, then `Finished`, then SessionStop | met | `session::shutdown` |
| V2G20-3205, 3206, 3207 to 3210, 3303, 3430 | Message sequence after the schedule exchange for AC, AC_DER, DC, MCS | met for AC and DC | `session_flow`, `against_evcc` |
| V2G20-3401 to 3405 | Every message family uses its own payload type | met | `PayloadType` constants |
| Deleted: V2G20-1235, 2646, 2371, 2180 to 2188 | TLS confirmed after the handshake (1237 stays), the inlet-voltage and "respect the limits" requirements | met (nothing to remove); comments updated | |
| V2G20-3171, 3172, 3180, 3181 | Three-phase AC: `_L2` and `_L3` elements come together, the choice is kept for the service session; the base element is L1 | open | The vehicle host (`Ev` limits) and the simulator charger give all three lines or none; nothing checks a peer. |
| Table 94 | New value range restrictions (signs of the discharge limits and others) | open | The simulator's limit checks cover the charge limits (V2G20-2198, 2199) only. |

## Security concept (clause 7.3, 7.7, Annex H)

| Requirement | Change | Status |
|---|---|---|
| V2G20-1006, 2401, 2402, 3373, 3374, 3381, 3382 | `certificate_authorities` extension in the ClientHello and CertificateRequest: the trust anchors' names, absent without roots, absent in CPM4PE | done: `Credentials::announce_authorities` (default on, `without_authorities()` for CPM4PE), `tls::peer_authorities` reads what the other side sent. Test: `the_trust_anchors_are_announced_as_certificate_authorities`; the interop scenarios with the Python partner pass with it on. OpenSSL's Rust crate has no wrapper for the client's CA list, so `tls.rs` declares two OpenSSL functions itself. |
| V2G20-3375 to 3378, 3383 to 3385 | Chain selection from the other side's list of roots; chain "up to a root, excluding it" | done: `Credentials::alternative(chain, key)` adds chains; a certificate callback (`SSL_CTX_set_cert_cb`, on the server after the ClientHello and on the client at the CertificateRequest) presents the first chain whose root the peer announced, the first of all when none or no list. Tests: `a_chain_is_chosen_by_the_roots_the_peer_announced` (both directions), `without_a_match_or_a_list_the_first_chain_is_presented`. Not wired into the simulators' settings. The chains handed in are sent as they are: the root is only left out if the PEM leaves it out (V2G20-3375, 3378, 3384, 3385). |
| V2G20-3379, 3380 | An OCSP response for each certificate in the chain | open as before: OpenSSL staples one response (see PLAN, TLS) |
| V2G20-3002 to 3005 | The OCSP signer's chain ends in the same root as the checked certificate, or in a root the other side listed | open; the signer is checked against the trust store only |
| V2G20-3388 to 3400 | Contract key transport for 456-bit (X448) keys, TPM key profiles | out of scope (certificate installation) |
| Certificate size 1 600 octets (V2G20-1234) | Limit on the size of a certificate | open; not checked |
| Cross certificates (Annex H.1.5, tables B.15, B.16) | Chains of four certificates, cross certificate profile | open; a chain of four certificates below the root (the length V2G20-1005 allows with a cross certificate) is reported as V2G20-1003 |
| Private environment (CPM4PE, tables B.17, B.18) | Profile and behaviour | open |

## New services

| Part | Status |
|---|---|
| AC DER IEC and SAE (payload types 0x8010, 0x8011, Annexes L, M, services 10 and 11) | open. The schemas are in `standards/AMD1_xsdSchema/` (`V2G_CI_AC_DER_IEC.xsd`, `V2G_CI_AC_DER_SAE.xsd`); the generator has no third and fourth namespace yet. |
| MCS and MCS_BPT (services 8 and 9, Annex K, IEC 61851-23-3 signalling, ISO 15118-10 10BASE-T1S, SDP over single-pair Ethernet) | open. Needs the CE state machine of IEC 61851-23-3 and a data link that is not PLC; the DC messages are reused (V2G20-3022), so the message side is the smaller part. V2G20-3006, 3007 (data link pause after `SessionStopReq` Pause) belong here. |
| ISO 15118-10 SDP (V2G20-3015 to 3019) | open |

## Order of work, if wanted

1. Generate the AC DER codecs and add the services to the lists; the vehicle and charger machines reuse the AC machines (V2G20-3030, 3176: "all requirements of AC apply as well"), so the parameters and the charge loop messages are the work.
2. MCS, when a CE-signalling test setup exists.
