# ISO 15118-3 requirements on the charger side

What ISO 15118-3:2015 asks of the charger's low-layer communication (clauses 6, 7, 9 and Annex A), and where it is. The matching machine is `evse_core::slac` (sans-IO, `no_std`); `evse-sim` still runs its own simulated responder (`evse_sim::slac`) which the machine is to replace.

Status: **done** (in the machine, tested), **host** (the host has to do it; the machine asks for it or reports it), **modem** (the modem's firmware, not the host's), **open**.

| Requirement | What | Status |
|---|---|---|
| V2G3-A09-03, 11, 12, 13 | Answers only with a vehicle on the pilot, only in "Unmatched", only inside `TT_EVSE_SLAC_init` from the trigger; `NoSlac` when none came | done |
| V2G3-A09-14, 35, 41, 47, 76, 98 | Content that deviates from the tables is ignored (the codec drops it) | done |
| V2G3-A09-15, 99 | The answers go out in the step that handles the request (`TP_match_response`, `TP_match_sequence` are the host's latency) | done |
| V2G3-A09-16 | A repeated request of a served vehicle starts its process over | done |
| V2G3-A09-19, 20, 43 to 45 | Average over the modem's `CM_ATTEN_PROFILE.IND`, corrected per group; calculation when all sounds are in or `TT_EVSE_match_MNBC` expires; `CM_ATTEN_CHAR.IND` with the count; a fixed attenuation without profiles | done |
| V2G3-A09-39, 40 | `TT_match_sequence` from the parameter answer; stopped by one start message; failure without it | done |
| V2G3-A09-46 | Report repeated `C_EV_match_retry` times on `TT_match_response`, then failed | done |
| V2G3-A09-48 to 53, 58, 75 to 90 | Validation: step 1 ready, not ready (another vehicle validating), not required or failure; repeated answer; `TT_match_sequence` and retries; step 2 counts for the vehicle's window; success with the count, failure when the host could not count | done (the counting is the host's, `Toggles`) |
| V2G3-A09-91 to 93, 99, 105 | Random key at every return to "Unmatched" (`Random`), `NID` from it, `CM_SLAC_MATCH.CNF`, `CM_SET_KEY` to the modem | done (the modem call is the host's) |
| V2G3-A09-96, 97, 118 | `TT_EVSE_match_session`; a repeated match request is answered, nothing else after "Matched" | done |
| V2G3-A09-103, 104 | `TT_match_join`; failure and a new key when no link | done |
| V2G3-A09-117, 120 | The higher layers are told the link is up after `TT_amp_map_exchange` | done |
| V2G3-A09-106 to 116, 119 | Amplitude map exchange | modem |
| V2G3-A09-121, M09-17, 18 | `D-LINK_TERMINATE.request`: new key (the modem leaves), "Unmatched", `D-LINK_READY(no link)` | done |
| V2G3-A09-126, 127, M09-19 | State A leaves the network and stops; E and F stop the matching | done |
| V2G3-M09-01 | `C_EVSE_match_parallel` = 5 processes at once | done |
| V2G3-M06-11 | The matching starts with a transition from A, E or F to B, C or D | done |
| V2G3-M06-07 | An AC charger without a request restarts the connection `C_sequ_retry` times through E/F, then X1 | done (`PilotRequest`; the pilot is the host's) |
| V2G3-M07-04 to 12 | `D-LINK_ERROR.request`: through X1 and E/F, or option B with a nominal duty cycle | done (`PilotRequest`) |
| V2G3-M07-03, 21, M12-01 | `D-LINK_READY.indication` on every change of the link | done |
| V2G3-M07-20, 22, 24 to 27 | Pause keeps the network parameters; wake-up configures the last ones; `T_conn_max_comm` for the link, then a restart | done (the B1/B2 transition of the wake-up is the host's) |
| V2G3-M06-05, 06, 08, 09, M07-02, 31 to 33 | The oscillator: E/F for `T_step_EF` when changing to a nominal duty cycle, X1 in the middle with a link, X1/X2 after EIM, the +12 V stays during a pause | host (`evse-sim`'s pilot, not yet in line with the machine) |
| V2G3-M09-10, 11 | The charger detects a toggle within `T_vald_detect_time` | host |
| V2G3-A06-03 | The module is the CCo | modem (PIB); a host driver can check it |
| V2G3-A06-04 | CAP3 for the matching frames | modem |
| V2G3-A10-01, M06-16 | Notched carriers, spectrum limits | modem |
| V2G3-M07-01, M08 | `T_conn_max_comm`: the module is ready 8 s after plug-in | host |
| V2G3-M09-05, 09 | The vehicle decides (signal strength, validation) | vehicle |
| ISO 15118-3 Annex B (G3-PLC) | Not implemented | open |

Not covered by anything here yet: a driver for the charger's modem (key, link detection, the profile messages), the host loop for the machine in `evse-sim`, scenarios against `pyslac` as the vehicle. See PLAN.md.
