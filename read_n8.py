#!/usr/bin/env python3
"""Read live channel metrics from an ISDT N8 over its hidraw interface."""

import argparse
import os
import select
import struct
import sys
import time


REPORT_SIZE = 64
REPORT_ID_OUT = 0x01
REPORT_ID_IN = 0x02
OPCODE_METRICS = 0xDE
RESPONSE_OPCODE_METRICS = 0xDF
METRICS_SIZE = 26


def make_request(channel: int) -> bytes:
    # ISDT frame: report ID, frame length, sync, host recipient, payload length,
    # opcode, zero-indexed channel, and the 8-bit sum of 12 02 DE channel.
    payload_sum = (0x12 + 0x02 + OPCODE_METRICS + channel) & 0xFF
    frame = bytes(
        [REPORT_ID_OUT, 0x06, 0xAA, 0x12, 0x02, OPCODE_METRICS, channel, payload_sum]
    )
    return frame + bytes(REPORT_SIZE - len(frame))


def read_metrics(fd: int, channel: int, timeout: float) -> bytes:
    # Discard any old report waiting in the queue so the reply belongs to this query.
    while select.select([fd], [], [], 0)[0]:
        os.read(fd, REPORT_SIZE)

    os.write(fd, make_request(channel))
    deadline = time.monotonic() + timeout

    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
            raise TimeoutError(f"Keine Antwort innerhalb von {timeout:g} Sekunden")

        report = os.read(fd, REPORT_SIZE)
        # HID report: ID, frame length, AA sync, charger recipient, payload length,
        # followed by the ISDT packet. The N8 may append additional bytes; the
        # standard metrics structure occupies the first 26 packet bytes.
        marker = bytes([0xAA, 0x21])
        start = report.find(marker)
        if start < 0 or len(report) < start + 5 + METRICS_SIZE:
            continue

        payload_start = start + 3
        payload_length = report[start + 2]
        packet = report[payload_start : payload_start + payload_length]
        if len(packet) < METRICS_SIZE or packet[0] != RESPONSE_OPCODE_METRICS:
            continue
        if packet[1] != channel:
            continue
        return packet[:METRICS_SIZE]


def decode(packet: bytes) -> dict:
    # N8 field layout confirmed against the display: progress, mV, mA, mOhm,
    # mW, mWh, mAh (or peak voltage), and seconds are little-endian values.
    return {
        "channel": packet[1],
        "mode": packet[2],
        "chemistry": packet[3],
        "size": packet[4],
        "temperature_c": packet[5],
        "internal_temperature_c": packet[6],
        "progress_percent": packet[7],
        "voltage_mv": struct.unpack_from("<H", packet, 8)[0],
        "current_ma": struct.unpack_from("<H", packet, 10)[0],
        "resistance_mohm": struct.unpack_from("<H", packet, 12)[0],
        "power_mw": struct.unpack_from("<H", packet, 14)[0],
        "energy_mwh": struct.unpack_from("<H", packet, 16)[0],
        "capacity_mah": struct.unpack_from("<I", packet, 18)[0],
        "time_s": struct.unpack_from("<I", packet, 22)[0],
    }


def format_metrics(values: dict) -> str:
    mode_names = {
        0: "Leerlauf",
        1: "Warten",
        2: "Verpolt",
        3: "Laden",
        4: "Geladen",
        5: "Entladen",
        6: "Entladen beendet",
        7: "Lagerspannung",
        8: "Lagerung beendet",
        9: "Zyklus",
        10: "Zyklus beendet",
        11: "Analyse",
        12: "Analyse beendet",
        13: "Aktivierung",
        15: "Entsorgen",
        17: "Abbruch",
    }
    status = mode_names.get(values["mode"], f"unbekannt ({values['mode']})")
    lines = [
        f"Fach/Kanal:             {values['channel'] + 1} (Protokollkanal {values['channel']})",
        f"Status:                 {status}",
        f"Ladefortschritt:        {values['progress_percent']} %",
        f"Spannung:               {values['voltage_mv'] / 1000:.3f} V",
        f"Strom:                  {values['current_ma'] / 1000:.3f} A",
        f"Widerstand:             {values['resistance_mohm']} mOhm",
        f"Temperatur:             {values['temperature_c']} °C",
        f"Ladekapazität:          {values['capacity_mah']} mAh",
        f"Energie:                {values['energy_mwh']} mWh",
        f"Leistung:               {values['power_mw'] / 1000:.3f} W",
        f"Ladezeit:               {values['time_s'] // 60}:{values['time_s'] % 60:02d} min",
        f"Akku-Chemie-ID:         {values['chemistry']}",
        f"Akkuformat-ID:          {values['size']}",
        f"Interne Temperatur:     {values['internal_temperature_c']} °C",
    ]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Liest Messwerte eines ISDT-N8-Ladefachs über USB (nur Statusabfrage)."
    )
    parser.add_argument("--device", default="/dev/hidraw0", help="hidraw-Gerät (Standard: /dev/hidraw0)")
    parser.add_argument("--channel", type=int, default=2, help="Fachnummer 1–8 (Standard: 2)")
    parser.add_argument("--watch", type=float, metavar="SEKUNDEN", help="Abfrage wiederholen, z. B. --watch 2")
    parser.add_argument("--timeout", type=float, default=2.0, help="Antwort-Timeout in Sekunden (Standard: 2)")
    args = parser.parse_args()

    if not 1 <= args.channel <= 8:
        parser.error("--channel muss zwischen 1 und 8 liegen")
    if args.watch is not None and args.watch <= 0:
        parser.error("--watch muss größer als 0 sein")

    channel = args.channel - 1
    try:
        fd = os.open(args.device, os.O_RDWR | os.O_NONBLOCK)
    except PermissionError:
        print(f"Kein Zugriff auf {args.device}; starte das Skript mit sudo.", file=sys.stderr)
        return 1
    except OSError as exc:
        print(f"Kann {args.device} nicht öffnen: {exc}", file=sys.stderr)
        return 1

    try:
        while True:
            try:
                packet = read_metrics(fd, channel, args.timeout)
                values = decode(packet)
                if args.watch is not None:
                    print("\033[2J\033[H", end="")
                print(format_metrics(values), flush=True)
                if args.watch is None:
                    break
                time.sleep(args.watch)
            except TimeoutError as exc:
                print(f"Abfrage fehlgeschlagen: {exc}", file=sys.stderr)
                return 2
            except KeyboardInterrupt:
                print("\nBeendet.")
                break
    finally:
        os.close(fd)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
