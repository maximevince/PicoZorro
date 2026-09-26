#!/usr/bin/env python3
"""Make a PicoZorro firmware update file (.pzf) or install UF2 from a firmware ELF (docs/UPDATE.md).

    tools/mkpzf.py firmware.elf out.pzf          # update image: try-before-you-buy set
    tools/mkpzf.py --bin --no-tbyb firmware.elf a.bin   # plain image for partition A (SWD install)
    tools/mkpzf.py --uf2 pt.bin firmware.elf install.uf2  # first install by UF2 drop (BOOTSEL)

The image is the ELF's flash contents from 0x10000000 (rust-objcopy -O binary).
Update images get the TBYB flag in their IMAGE_DEF (picobin.h
PICOBIN_IMAGE_TYPE_EXE_TBYB_BITS; datasheet 5.1.17): after the flash update
reboot the bootrom runs them on trial, and only `pzflash CONFIRM` (explicit_buy)
keeps them. The image carries no hash or signature, so the bit can be set after
linking.

The install UF2 carries what tools/pz_install.sh writes over SWD: the
partition table (`picotool partition create firmware/pt.json pt.bin`) at
0x10000000, the image with TBYB off in partition A at 0x10010000, and one
erased sector at the start of partition B, 0x10210000. Its blocks have the
`absolute` family, so the bootrom writes them at their addresses whatever
partition table the flash holds (datasheet 5.1.18, 5.5.3; uf2.h).

.pzf layout (big-endian, 64-byte header, then the image):
    0  4  magic "PZF1"
    4  4  image length
    8  4  CRC-32 (zlib) of the image
   12  4  flags: bit 0 = TBYB set
   16 16  version string of the image, NUL-padded (FW_VERSION after the update)
   32 32  reserved, 0
"""
import argparse
import struct
import subprocess
import sys
import tempfile
import zlib

BLOCK_START = 0xFFFFDED3
IMAGE_TYPE_ITEM = 0x42
TBYB = 0x8000

UF2_MAGIC_START0 = 0x0A324655
UF2_MAGIC_START1 = 0x9E5D5157
UF2_MAGIC_END = 0x0AB16F30
UF2_FLAG_FAMILY_ID_PRESENT = 0x00002000
ABSOLUTE_FAMILY_ID = 0xE48BFF57
UF2_PAYLOAD = 256
# Flash addresses, firmware/pt.json: table at 0, A at 64 KiB, B at 64 + 2048 KiB.
PT_ADDR = 0x10000000
A_ADDR = 0x10010000
B_ADDR = 0x10210000


def elf_to_bin(elf):
    with tempfile.NamedTemporaryFile(suffix=".bin") as t:
        subprocess.run(["rust-objcopy", "-O", "binary", elf, t.name], check=True)
        return bytearray(open(t.name, "rb").read())


def set_tbyb(img, on):
    """Set or clear TBYB in the first IMAGE_DEF's IMAGE_TYPE item (first 4 KiB)."""
    for off in range(0, min(len(img), 4096) - 8, 4):
        if struct.unpack_from("<I", img, off)[0] != BLOCK_START:
            continue
        item = struct.unpack_from("<I", img, off + 4)[0]
        if item & 0xFF == IMAGE_TYPE_ITEM and (item >> 8) & 0xFF == 1:
            flags = item >> 16
            flags = (flags | TBYB) if on else (flags & ~TBYB)
            struct.pack_into("<I", img, off + 4, (item & 0xFFFF) | flags << 16)
            return off, flags
    raise SystemExit("mkpzf: no IMAGE_DEF with an IMAGE_TYPE item in the first 4 KiB")


def version_of(img):
    """The version string the firmware carries after its "PZVERSN:" tag
    (pz-app update_task.rs VERSION_TAG), for the header."""
    i = bytes(img).find(b"PZVERSN:")
    return bytes(img[i + 8:i + 24]).rstrip(b"\0") if i >= 0 else b""


def uf2(segments):
    """Absolute-family UF2 of (address, data) segments, 256-byte payloads,
    the last one of each segment padded with 0xFF."""
    blocks = []
    for addr, data in segments:
        data = bytes(data) + b"\xff" * (-len(data) % UF2_PAYLOAD)
        for o in range(0, len(data), UF2_PAYLOAD):
            blocks.append((addr + o, data[o:o + UF2_PAYLOAD]))
    out = bytearray()
    for n, (addr, payload) in enumerate(blocks):
        hdr = struct.pack("<8I", UF2_MAGIC_START0, UF2_MAGIC_START1, UF2_FLAG_FAMILY_ID_PRESENT,
                          addr, UF2_PAYLOAD, n, len(blocks), ABSOLUTE_FAMILY_ID)
        out += hdr + payload.ljust(476, b"\0") + struct.pack("<I", UF2_MAGIC_END)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("elf")
    ap.add_argument("out")
    ap.add_argument("--bin", action="store_true", help="write the plain image, no header")
    ap.add_argument("--no-tbyb", action="store_true", help="leave try-before-you-buy off")
    ap.add_argument("--uf2", metavar="PT_BIN",
                    help="write the install UF2 with this partition table binary (TBYB off)")
    ap.add_argument("--version", help="version string for the header (default: found in the image)")
    a = ap.parse_args()
    img = elf_to_bin(a.elf)
    off, flags = set_tbyb(img, not (a.no_tbyb or a.uf2))
    if a.uf2:
        pt = open(a.uf2, "rb").read()
        if PT_ADDR + len(pt) > A_ADDR or A_ADDR + len(img) > B_ADDR:
            raise SystemExit("mkpzf: partition table or image larger than its space in pt.json")
        out = uf2([(PT_ADDR, pt), (A_ADDR, img), (B_ADDR, b"\xff" * 4096)])
        open(a.out, "wb").write(out)
        print("mkpzf: %s: %d UF2 blocks, table %d bytes, image %d bytes, IMAGE_DEF at %#x, image type %04x" % (
            a.out, len(out) // 512, len(pt), len(img), off, flags))
        return
    if a.bin:
        open(a.out, "wb").write(img)
    else:
        ver = a.version.encode() if a.version else version_of(img)
        crc = zlib.crc32(img) & 0xFFFFFFFF
        hdr = struct.pack(">4sIII16s32s", b"PZF1", len(img), crc, 0 if a.no_tbyb else 1, ver, b"")
        open(a.out, "wb").write(hdr + img)
    print("mkpzf: %s: %d bytes, crc32 %08x, IMAGE_DEF at %#x, image type %04x%s" % (
        a.out, len(img), zlib.crc32(img) & 0xFFFFFFFF, off, flags, "" if a.bin else ", version %r" % version_of(img).decode()))


if __name__ == "__main__":
    main()
