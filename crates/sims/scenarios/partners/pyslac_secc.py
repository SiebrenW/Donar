"""The EVSE side of SLAC of the pyslac library on one interface, for the scenario runner.

There is no PLC chip in the sandbox, so the chip is played here: `CM_SET_KEY` is not sent (the key and network id
are made the way the library makes them and kept), and every `CM_MNBC_SOUND.IND` of the vehicle is answered with the
`CM_ATTEN_PROFILE.IND` the chip would hand to the host. The library does the rest (CM_SLAC_PARM, the sounds,
CM_ATTEN_CHAR, CM_SLAC_MATCH).
Prints `pyslac ready` when it waits for the vehicle and `pyslac matched` when the match was answered.
"""
import asyncio
import logging
import socket
import sys
from os import urandom

from pyslac.enums import CM_ATTEN_PROFILE, CM_MNBC_SOUND, ETH_TYPE_HPAV, EVSE_PLC_MAC, MMTYPE_IND
from pyslac.environment import Config
from pyslac.layer_2_headers import EthernetHeader, HomePlugHeader
from pyslac.messages import AttenProfile
from pyslac.session import STATE_MATCHED, SlacEvseSession, SlacSessionController
from pyslac.utils import generate_nid

logging.basicConfig(level=logging.DEBUG)


async def plc_chip(interface: str, host_mac: bytes):
    """The EVSE's PLC chip: reports the attenuation of each sound to the host."""
    sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETH_TYPE_HPAV))
    sock.bind((interface, 0))
    sock.setblocking(False)
    loop = asyncio.get_running_loop()
    while True:
        data = await loop.sock_recv(sock, 2048)
        if len(data) < 17 or data[6:12] == host_mac or data[6:12] == EVSE_PLC_MAC:
            continue
        if int.from_bytes(data[15:17], "little") == CM_MNBC_SOUND | MMTYPE_IND:
            pev_mac = data[6:12]
            report = AttenProfile(pev_mac=pev_mac, aag=[5] * 58)
            frame = (
                EthernetHeader(dst_mac=host_mac, src_mac=EVSE_PLC_MAC).pack_big()
                + HomePlugHeader(CM_ATTEN_PROFILE | MMTYPE_IND).pack_big()
                + report.pack_big()
            )
            sock.send(frame)


async def main(interface: str) -> int:
    config = Config()
    config.load_envs()
    session = SlacEvseSession("DE*SIM*E1", interface, config)

    async def set_key():
        session.nmk = urandom(16)
        session.nid = generate_nid(session.nmk)

    session.evse_set_key = set_key
    await set_key()
    chip = asyncio.create_task(plc_chip(interface, session.evse_mac))
    controller = SlacSessionController()
    print(f"pyslac key: nid {session.nid.hex()} nmk {session.nmk.hex()}", flush=True)
    print("pyslac ready", flush=True)
    # The control pilot goes to state B: the matching starts.
    await controller.process_cp_state(session, "B")
    for _ in range(600):
        await asyncio.sleep(0.1)
        if session.state == STATE_MATCHED:
            print("pyslac matched", flush=True)
            await asyncio.sleep(30)
            return 0
    print("pyslac: no match", flush=True)
    return 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1])))
