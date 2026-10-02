/*
 * memdump: copy a range of the address space into a file, for looking at
 * expansion ROMs and the like from the PC (memdump ADDR LEN FILE, hex).
 */
#include <exec/types.h>
#include <dos/dos.h>
#include <proto/exec.h>
#include <proto/dos.h>

static ULONG hex(const char *s)
{
    ULONG v = 0;
    if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X'))
        s += 2;
    else if (s[0] == '$')
        s++;
    for (; *s; s++) {
        char c = *s | 0x20;
        v = (v << 4) | (c >= 'a' ? c - 'a' + 10 : c - '0');
    }
    return v;
}

int main(int argc, char **argv)
{
    ULONG addr, len, i;
    UWORD *src, *buf;
    BPTR f;

    if (argc != 4) {
        PutStr("usage: memdump ADDR LEN FILE (hex)\n");
        return RETURN_ERROR;
    }
    addr = hex(argv[1]) & ~1UL;
    len = (hex(argv[2]) + 1) & ~1UL;
    buf = AllocVec(len, MEMF_ANY);
    if (!buf)
        return RETURN_FAIL;
    /* Word reads, as a 16-bit expansion bus wants them. */
    src = (UWORD *)addr;
    for (i = 0; i < len / 2; i++)
        buf[i] = src[i];
    f = Open(argv[3], MODE_NEWFILE);
    if (f) {
        Write(f, buf, len);
        Close(f);
    }
    FreeVec(buf);
    return f ? RETURN_OK : RETURN_ERROR;
}
