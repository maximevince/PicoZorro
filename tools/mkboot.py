#!/usr/bin/env python3
"""Build the firmware's boot image: the boot ROM (amiga/bootrom/boot.bin)
and the Amiga modules it loads, parsed from their AmigaOS hunk files, in
the format pz_core::boot::Image reads:

    "PZB1", modules.w, rom_len.w, rom
    per module: hunks.w, romtag_hunk.w, romtag_offset.l,
      per hunk: mem_bytes.l, memf.l, data_bytes.l, data,
                relocs.l, relocs x (target_hunk.l, offset.l)

    tools/mkboot.py -o firmware/boot.img amiga/bootrom/boot.bin \\
        amiga/picozorro.device/picozorro.device ...

The module's romtag is the $4AFC whose rt_MatchTag is relocated to itself.
"""
import argparse
import struct
import sys

HUNK_CODE, HUNK_DATA, HUNK_BSS = 0x3E9, 0x3EA, 0x3EB
HUNK_RELOC32, HUNK_SYMBOL, HUNK_DEBUG, HUNK_END, HUNK_HEADER = 0x3EC, 0x3F0, 0x3F1, 0x3F2, 0x3F3
HUNK_RELOC32SHORT, HUNK_DREL32 = 0x3FC, 0x3F7
MEMF_PUBLIC, MEMF_CHIP, MEMF_FAST, MEMF_CLEAR = 1, 2, 4, 0x10000
STREAM_BYTES = 48 * 1024   # pz_core::boot::STREAM_BYTES
MAX_ADDRS = 32


class Reader:
    def __init__(self, d):
        self.d, self.at = d, 0

    def long(self):
        v = struct.unpack_from(">I", self.d, self.at)[0]
        self.at += 4
        return v

    def word(self):
        v = struct.unpack_from(">H", self.d, self.at)[0]
        self.at += 2
        return v

    def take(self, n):
        v = self.d[self.at:self.at + n]
        self.at += n
        return v


def load_hunks(path):
    """[(mem_bytes, memf, data, relocs[(target, offset)])]"""
    r = Reader(open(path, "rb").read())
    if r.long() != HUNK_HEADER:
        raise SystemExit("mkboot: %s: not a load file" % path)
    n = r.long()                        # resident library names (none)
    while n:
        r.take(4 * n)
        n = r.long()
    count, first, last = r.long(), r.long(), r.long()
    sizes = []
    for _ in range(last - first + 1):
        v = r.long()
        memf = MEMF_PUBLIC | MEMF_CLEAR
        if v & 0xC0000000 == 0xC0000000:
            memf |= r.long()            # extended flags
        elif v & 0x40000000:
            memf |= MEMF_CHIP
        elif v & 0x80000000:
            memf |= MEMF_FAST
        sizes.append(((v & 0x3FFFFFFF) * 4, memf))
    hunks = []
    cur = None
    while r.at < len(r.d):
        t = r.long() & 0x3FFFFFFF
        if t in (HUNK_CODE, HUNK_DATA):
            n = r.long() * 4
            cur = [sizes[len(hunks)][0], sizes[len(hunks)][1], r.take(n), []]
        elif t == HUNK_BSS:
            r.long()
            cur = [sizes[len(hunks)][0], sizes[len(hunks)][1], b"", []]
        elif t == HUNK_RELOC32:
            while True:
                n = r.long()
                if not n:
                    break
                target = r.long()
                cur[3].extend((target, r.long()) for _ in range(n))
        elif t in (HUNK_RELOC32SHORT, HUNK_DREL32):
            words = 0
            while True:
                n = r.word()
                words += 1
                if not n:
                    break
                target = r.word()
                words += 1
                for _ in range(n):
                    cur[3].append((target, r.word()))
                words += n
            if words % 2:
                r.word()
        elif t == HUNK_SYMBOL:
            while True:
                n = r.long()
                if not n:
                    break
                r.take(4 * (n + 1))
        elif t == HUNK_DEBUG:
            r.take(4 * r.long())
        elif t == HUNK_END:
            hunks.append(tuple(cur))
            cur = None
        else:
            raise SystemExit("mkboot: %s: hunk type %#x not handled" % (path, t))
    if len(hunks) != count:
        raise SystemExit("mkboot: %s: %d hunks, header says %d" % (path, len(hunks), count))
    return hunks


def find_romtag(path, hunks):
    for h, (_, _, data, relocs) in enumerate(hunks):
        own = {off for target, off in relocs if target == h}
        for o in range(0, len(data) - 6, 2):
            if data[o:o + 2] == b"\x4a\xfc" and o + 2 in own and struct.unpack_from(">I", data, o + 2)[0] == o:
                return h, o
    raise SystemExit("mkboot: %s: no romtag" % path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-o", "--out", required=True)
    ap.add_argument("rom")
    ap.add_argument("modules", nargs="*")
    a = ap.parse_args()
    rom = open(a.rom, "rb").read()
    if len(rom) > 256 or len(rom) % 2:
        raise SystemExit("mkboot: the boot ROM must be an even number of bytes, 256 at most")
    if struct.unpack_from(">H", rom, 2)[0] != len(rom):
        raise SystemExit("mkboot: da_Size is not the ROM's length")
    out = b"PZB1" + struct.pack(">HH", len(a.modules), len(rom)) + rom
    stream, addrs = 2, 0
    for path in a.modules:
        hunks = load_hunks(path)
        th, to = find_romtag(path, hunks)
        out += struct.pack(">HHI", len(hunks), th, to)
        stream += 2 + 8 * len(hunks) + 4
        for mem, memf, data, relocs in hunks:
            if len(data) % 2:
                data += b"\0"
            out += struct.pack(">III", mem, memf, len(data)) + data + struct.pack(">I", len(relocs))
            out += b"".join(struct.pack(">II", t, o) for t, o in relocs)
            stream += 8 + len(data)
        addrs += len(hunks)
        print("mkboot: %s: %d hunks, %d bytes of memory, romtag in hunk %d at %d"
              % (path, len(hunks), sum(h[0] for h in hunks), th, to))
    stream += 2
    if stream > STREAM_BYTES or addrs > MAX_ADDRS:
        raise SystemExit("mkboot: the stream needs %d bytes and %d hunks (%d, %d fit)"
                         % (stream, addrs, STREAM_BYTES, MAX_ADDRS))
    open(a.out, "wb").write(out)
    print("mkboot: %s: %d bytes, ROM %d bytes, stream %d of %d bytes" % (a.out, len(out), len(rom), stream, STREAM_BYTES))


if __name__ == "__main__":
    main()
