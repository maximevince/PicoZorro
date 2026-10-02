/*
 * devinfo: the mount parameters of every filesystem device in the DosList
 * (driver, unit, geometry, buffers, BufMemType, MaxTransfer, Mask, DosType).
 * Read-only, for diagnosing disk trouble on a real machine.
 */
#include <exec/types.h>
#include <dos/dos.h>
#include <dos/dosextens.h>
#include <dos/filehandler.h>
#include <proto/exec.h>
#include <proto/dos.h>

static void bstr(BSTR b, char *out, int max)
{
    const UBYTE *s = (const UBYTE *)BADDR(b);
    int n = s ? s[0] : 0;
    if (n > max - 1)
        n = max - 1;
    while (n--)
        *out++ = *++s;
    *out = 0;
}

int main(void)
{
    struct DosList *dl;
    char name[64], dev[64];

    dl = LockDosList(LDF_DEVICES | LDF_READ);
    while ((dl = NextDosEntry(dl, LDF_DEVICES)) != NULL) {
        struct FileSysStartupMsg *fssm;
        struct DosEnvec *de;

        bstr(dl->dol_Name, name, sizeof(name));
        /* Handlers without a FileSysStartupMsg keep a small number or 0 here. */
        if ((ULONG)dl->dol_misc.dol_handler.dol_Startup < 1024)
            continue;
        fssm = (struct FileSysStartupMsg *)BADDR(dl->dol_misc.dol_handler.dol_Startup);
        if (TypeOfMem(fssm) == 0 || fssm->fssm_Environ == 0)
            continue;
        de = (struct DosEnvec *)BADDR(fssm->fssm_Environ);
        if (TypeOfMem(de) == 0 || de->de_TableSize < DE_DOSTYPE)
            continue;
        bstr(fssm->fssm_Device, dev, sizeof(dev));
        Printf("%s: %s unit %ld flags %lx\n", name, dev, fssm->fssm_Unit, fssm->fssm_Flags);
        Printf("  SizeBlock %ld Surfaces %ld BlocksPerTrack %ld Reserved %ld LowCyl %ld HighCyl %ld\n",
               de->de_SizeBlock, de->de_Surfaces, de->de_BlocksPerTrack, de->de_Reserved,
               de->de_LowCyl, de->de_HighCyl);
        Printf("  NumBuffers %ld BufMemType %ld MaxTransfer 0x%lx Mask 0x%lx BootPri %ld DosType 0x%08lx\n",
               de->de_NumBuffers, de->de_BufMemType, de->de_MaxTransfer, de->de_Mask,
               de->de_BootPri, de->de_DosType);
    }
    UnLockDosList(LDF_DEVICES | LDF_READ);
    return RETURN_OK;
}
